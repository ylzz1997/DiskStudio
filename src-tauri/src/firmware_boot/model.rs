use super::{catalog::MAX_DISCOVERED, Change, Entry, EntryStatus, Report};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

const MAX_ENTRIES: usize = 512;
const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_VARIABLE_SIZE: usize = 65_536;
const ACTIVE: u32 = 1;
const CATEGORY: u32 = 0x1f00;
const BOOT_TO_FIRMWARE_UI: u64 = 1;
pub(super) const BOOT_VARIABLE_ATTRIBUTES: u32 = 7;
pub(super) const STALE: &str = "固件启动项已有变化，请重新读取后预览。";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Variable {
    Order,
    Current,
    Next,
    Indications,
    IndicationsSupported,
    Entry(u16),
}

impl Variable {
    pub(super) fn name(self) -> String {
        match self {
            Self::Order => "BootOrder".into(),
            Self::Current => "BootCurrent".into(),
            Self::Next => "BootNext".into(),
            Self::Indications => "OsIndications".into(),
            Self::IndicationsSupported => "OsIndicationsSupported".into(),
            Self::Entry(id) => format!("Boot{id:04X}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Value {
    pub(super) attributes: u32,
    pub(super) bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Snapshot {
    values: BTreeMap<Variable, Value>,
    complete_catalog: bool,
}

struct LoadOption {
    flags: u32,
    description: String,
    description_bytes: Range<usize>,
    device_path: Range<usize>,
    file_paths: Vec<String>,
    gpt_partitions: Vec<uuid::Uuid>,
}

impl LoadOption {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let flags = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
        let path_size = u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?) as usize;
        let (description, description_len) = utf16z(bytes.get(6..)?)?;
        let path_start = 6 + description_len;
        let path = bytes.get(path_start..path_start.checked_add(path_size)?)?;
        let mut remainder = path;
        let mut file_paths = Vec::new();
        let mut gpt_partitions = Vec::new();
        let mut instance_has_node = false;
        let mut primary_path = true;
        while !remainder.is_empty() {
            let header = remainder.get(..4)?;
            let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
            if size < 4 {
                return None;
            }
            let (node, tail) = remainder.split_at_checked(size)?;
            match (node[0], node[1]) {
                (0x7f, 0xff) if size == 4 && instance_has_node => {
                    if tail.is_empty() {
                        return Some(Self {
                            flags,
                            description,
                            description_bytes: 6..path_start,
                            device_path: path_start..path_start + path_size,
                            file_paths,
                            gpt_partitions,
                        });
                    }
                    primary_path = false;
                    instance_has_node = false;
                }
                (0x7f, 1) if size == 4 && !tail.is_empty() && instance_has_node => {
                    instance_has_node = false;
                }
                (0x7f, _) => return None,
                (4, 4) => {
                    let (file_path, consumed) = utf16z(&node[4..])?;
                    if consumed != size - 4 || file_path.is_empty() {
                        return None;
                    }
                    if primary_path {
                        file_paths.push(file_path);
                    }
                    instance_has_node = true;
                }
                (4, 1) => {
                    if size != 42 {
                        return None;
                    }
                    if primary_path && node[40..42] == [2, 2] {
                        let id = uuid::Uuid::from_bytes_le(node[24..40].try_into().ok()?);
                        gpt_partitions.push(id);
                    }
                    instance_has_node = true;
                }
                _ => instance_has_node = true,
            }
            remainder = tail;
        }
        None
    }

    fn active(&self) -> bool {
        self.flags & ACTIVE != 0
    }

    fn boot_category(&self) -> bool {
        self.flags & CATEGORY == 0
    }
}

fn utf16z(bytes: &[u8]) -> Option<(String, usize)> {
    let mut units = Vec::new();
    for pair in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            let length = (units.len() + 1) * 2;
            return Some((String::from_utf16(&units).ok()?, length));
        }
        units.push(unit);
    }
    None
}

fn encode_ids(ids: &[u16]) -> Vec<u8> {
    ids.iter().flat_map(|id| id.to_le_bytes()).collect()
}

fn boot_value(bytes: Vec<u8>) -> Value {
    Value {
        attributes: BOOT_VARIABLE_ATTRIBUTES,
        bytes,
    }
}

fn description_bytes(description: &str) -> Result<impl Iterator<Item = u8> + '_, String> {
    if description.trim().is_empty()
        || description.chars().any(char::is_control)
        || description.encode_utf16().take(256).count() > 255
    {
        return Err("启动项名称需要 1 至 255 个 UTF-16 代码单元，且不能含控制字符。".into());
    }
    Ok(description
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes))
}

pub(super) fn gpt_device_path(
    path: &[String],
    slot: u32,
    partition: uuid::Uuid,
    start_lba: u64,
    blocks: u64,
) -> Result<Vec<u8>, String> {
    if slot == 0
        || partition.is_nil()
        || start_lba == 0
        || blocks == 0
        || path.is_empty()
        || path.iter().any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.contains(['/', '\\', ':'])
                || part.chars().any(char::is_control)
        })
    {
        return Err("EFI 文件路径或 GPT 位置无效。".into());
    }
    let file = format!("\\{}", path.join("\\"));
    let file: Vec<u8> = file
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    let node_size = u16::try_from(file.len() + 4).map_err(|_| "EFI 文件路径过长。")?;
    let path_size = node_size.checked_add(46).ok_or("EFI 设备路径过长。")?;
    let mut bytes = Vec::with_capacity(usize::from(path_size));
    bytes.extend([4, 1, 42, 0]);
    bytes.extend(slot.to_le_bytes());
    bytes.extend(start_lba.to_le_bytes());
    bytes.extend(blocks.to_le_bytes());
    bytes.extend(partition.to_bytes_le());
    bytes.extend([2, 2, 4, 4]);
    bytes.extend(node_size.to_le_bytes());
    bytes.extend(file);
    bytes.extend([0x7f, 0xff, 4, 0]);
    Ok(bytes)
}

pub(super) fn new_load_option(description: &str, path: &[u8]) -> Result<Value, String> {
    let mut bytes = ACTIVE.to_le_bytes().to_vec();
    bytes.extend(path_length(path)?);
    bytes.extend(description_bytes(description)?);
    bytes.extend(path);
    let value = boot_value(bytes);
    validate_entry(&value)?;
    Ok(value)
}

fn path_length(path: &[u8]) -> Result<[u8; 2], String> {
    u16::try_from(path.len())
        .map(u16::to_le_bytes)
        .map_err(|_| "EFI 设备路径过长。".into())
}

pub(super) fn validate_entry(value: &Value) -> Result<(), String> {
    if value.attributes != BOOT_VARIABLE_ATTRIBUTES
        || value.bytes.len() > MAX_VARIABLE_SIZE
        || LoadOption::parse(&value.bytes).is_none()
    {
        return Err("UEFI 启动项格式或固件属性无效。".into());
    }
    Ok(())
}

impl Snapshot {
    pub(super) fn boot_entry(&self, id: u16) -> Option<&Value> {
        self.values.get(&Variable::Entry(id))
    }

    #[cfg(test)]
    pub(super) fn capture(
        read: impl FnMut(Variable) -> Result<Option<Value>, String>,
    ) -> Result<Self, String> {
        Self::capture_catalog(read, || Ok(None))
    }

    pub(super) fn capture_catalog(
        read: impl FnMut(Variable) -> Result<Option<Value>, String>,
        mut enumerate: impl FnMut() -> Result<Option<BTreeSet<u16>>, String>,
    ) -> Result<Self, String> {
        Self::capture_watching(read, &mut enumerate, &BTreeSet::new())
    }

    fn capture_watching(
        mut read: impl FnMut(Variable) -> Result<Option<Value>, String>,
        enumerate: &mut impl FnMut() -> Result<Option<BTreeSet<u16>>, String>,
        watch: &BTreeSet<u16>,
    ) -> Result<Self, String> {
        let selectors = [
            Variable::Order,
            Variable::Current,
            Variable::Next,
            Variable::Indications,
            Variable::IndicationsSupported,
        ];
        let catalog = enumerate()?;
        let mut snapshot = Self {
            values: BTreeMap::new(),
            complete_catalog: catalog.is_some(),
        };
        for key in selectors {
            if let Some(value) = read(key)? {
                snapshot.values.insert(key, value);
            }
        }
        let mut ids = snapshot.referenced()?;
        ids.extend(watch);
        if let Some(names) = &catalog {
            ids.extend(names.iter().copied());
        }
        let mut bytes_read = 0;
        for id in ids {
            let key = Variable::Entry(id);
            let value = read(key)?;
            if let Some(names) = &catalog {
                if value.is_some() != names.contains(&id) {
                    return Err(STALE.into());
                }
            }
            if let Some(value) = value {
                bytes_read += value.bytes.len();
                if bytes_read > MAX_CAPTURE_BYTES {
                    return Err("固件启动项数据超过读取上限。".into());
                }
                snapshot.values.insert(key, value);
            }
        }
        for key in selectors {
            if read(key)?.as_ref() != snapshot.values.get(&key) {
                return Err(STALE.into());
            }
        }
        if enumerate()? != catalog {
            return Err(STALE.into());
        }
        Ok(snapshot)
    }

    fn order(&self) -> Result<Vec<u16>, String> {
        let Some(value) = self.values.get(&Variable::Order) else {
            return Ok(Vec::new());
        };
        if value.bytes.len() % 2 != 0 || value.bytes.len() > MAX_ENTRIES * 2 {
            return Err("UEFI 启动顺序的长度无效。".into());
        }
        let order: Vec<_> = value
            .bytes
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        if order.iter().copied().collect::<BTreeSet<_>>().len() != order.len() {
            return Err("UEFI 启动顺序包含重复项。".into());
        }
        Ok(order)
    }

    fn selected(&self, key: Variable) -> Result<Option<u16>, String> {
        self.values
            .get(&key)
            .map(|value| {
                value
                    .bytes
                    .as_slice()
                    .try_into()
                    .map(u16::from_le_bytes)
                    .map_err(|_| format!("{} 固件变量的长度无效。", key.name()))
            })
            .transpose()
    }

    fn referenced(&self) -> Result<BTreeSet<u16>, String> {
        let mut ids: BTreeSet<_> = self.order()?.into_iter().collect();
        ids.extend(self.selected(Variable::Current)?);
        ids.extend(self.selected(Variable::Next)?);
        Ok(ids)
    }

    fn load_option(&self, id: u16) -> Option<LoadOption> {
        self.values
            .get(&Variable::Entry(id))
            .and_then(|value| LoadOption::parse(&value.bytes))
    }

    fn revision(&self) -> Result<String, String> {
        let ids = self.referenced()?;
        let mut digest = Sha256::new();
        digest.update([u8::from(self.complete_catalog)]);
        for (key, value) in &self.values {
            if !self.complete_catalog && matches!(key, Variable::Entry(id) if !ids.contains(id)) {
                continue;
            }
            let name = key.name();
            digest.update((name.len() as u32).to_le_bytes());
            digest.update(name);
            digest.update(value.attributes.to_le_bytes());
            digest.update((value.bytes.len() as u32).to_le_bytes());
            digest.update(&value.bytes);
        }
        Ok(format!("{:x}", digest.finalize()))
    }

    pub(super) fn report(&self) -> Result<Report, String> {
        let order = self.order()?;
        let current = self.selected(Variable::Current)?;
        let next = self.selected(Variable::Next)?;
        let mut ids = order.clone();
        for id in [current, next].into_iter().flatten() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if self.complete_catalog {
            let included: BTreeSet<_> = ids.iter().copied().collect();
            ids.extend(self.values.keys().filter_map(|key| match key {
                Variable::Entry(id) if !included.contains(id) => Some(*id),
                _ => None,
            }));
        }
        let entries = ids
            .into_iter()
            .map(|id| {
                let value = self.values.get(&Variable::Entry(id));
                let (status, option) = match value {
                    None => (EntryStatus::Missing, None),
                    Some(value) => match LoadOption::parse(&value.bytes) {
                        Some(option) => (EntryStatus::Ready, Some(option)),
                        None => (EntryStatus::Invalid, None),
                    },
                };
                let active = option.as_ref().map(LoadOption::active);
                let boot_category = option.as_ref().is_some_and(LoadOption::boot_category);
                let parameters_bytes = option
                    .as_ref()
                    .zip(value)
                    .map(|(entry, value)| value.bytes.len() - entry.device_path.end);
                let (description, file_paths, gpt_partitions) = option
                    .map(|entry| {
                        (
                            Some(entry.description),
                            entry.file_paths,
                            entry.gpt_partitions,
                        )
                    })
                    .unwrap_or_default();
                Entry {
                    id,
                    status,
                    description,
                    active,
                    boot_category,
                    file_paths,
                    gpt_partitions,
                    parameters_bytes,
                }
            })
            .collect();
        Ok(Report {
            revision: self.revision()?,
            order,
            current,
            next,
            all_entries: self.complete_catalog,
            setup_next: self
                .setup_indications()
                .map(|value| value & BOOT_TO_FIRMWARE_UI != 0),
            entries,
        })
    }

    fn writable(&self, key: Variable) -> Result<Option<&Value>, String> {
        let value = self.values.get(&key);
        if value.is_some_and(|value| value.attributes != BOOT_VARIABLE_ATTRIBUTES) {
            return Err(format!("{} 的固件变量属性不支持修改。", key.name()));
        }
        Ok(value)
    }

    fn editable_option(&self, id: u16) -> Result<(&Value, LoadOption), String> {
        let key = Variable::Entry(id);
        let value = self.writable(key)?.ok_or("所选固件启动项不存在。")?;
        let option = LoadOption::parse(&value.bytes).ok_or("所选固件启动项的设备路径无效。")?;
        Ok((value, option))
    }

    fn has_active_boot_entry(&self) -> Result<bool, String> {
        Ok(self
            .order()?
            .into_iter()
            .any(|id| self.active_boot_entry(id)))
    }

    fn active_boot_entry(&self, id: u16) -> bool {
        self.load_option(id)
            .is_some_and(|entry| entry.boot_category() && entry.active())
    }

    fn setup_indications(&self) -> Option<u64> {
        let supported = self.values.get(&Variable::IndicationsSupported)?;
        if supported.attributes != 6
            || u64::from_le_bytes(supported.bytes.as_slice().try_into().ok()?) & BOOT_TO_FIRMWARE_UI
                == 0
        {
            return None;
        }
        match self.values.get(&Variable::Indications) {
            None => Some(0),
            Some(value) if value.attributes == BOOT_VARIABLE_ATTRIBUTES => {
                Some(u64::from_le_bytes(value.bytes.as_slice().try_into().ok()?))
            }
            _ => None,
        }
    }

    pub(super) fn prepare(self, revision: &str, change: &Change) -> Result<Plan, String> {
        self.check_revision(revision)?;
        let (variable, replacement) = match change {
            Change::Order { entries } => {
                let order = self.order()?;
                if order.is_empty()
                    || entries.len() != order.len()
                    || entries.iter().copied().collect::<BTreeSet<_>>()
                        != order.iter().copied().collect()
                    || entries.iter().any(|id| self.load_option(*id).is_none())
                {
                    return Err("新顺序必须完整保留可读取的现有启动项。".into());
                }
                (Variable::Order, Some(boot_value(encode_ids(entries))))
            }
            Change::Once { entry } => {
                let option = self
                    .load_option(*entry)
                    .ok_or("所选固件启动项不存在或无效。")?;
                if !option.active() || !option.boot_category() {
                    return Err("一次性启动需要已启用的启动项。".into());
                }
                (Variable::Next, Some(boot_value(encode_ids(&[*entry]))))
            }
            Change::ClearOnce => (Variable::Next, None),
            Change::Active { entry, active } => {
                let (value, option) = self.editable_option(*entry)?;
                if !option.boot_category() {
                    return Err("仅可启用或停用固件的启动类别项。".into());
                }
                if !active {
                    let current = self
                        .selected(Variable::Current)?
                        .ok_or("当前固件启动项不可用。")?;
                    if *entry == current || self.selected(Variable::Next)? == Some(*entry) {
                        return Err("当前或已安排一次性启动的项不能停用。".into());
                    }
                }
                let mut replacement = value.clone();
                let flags = (option.flags & !ACTIVE) | u32::from(*active);
                replacement.bytes[..4].copy_from_slice(&flags.to_le_bytes());
                (Variable::Entry(*entry), Some(replacement))
            }
            Change::Rename { entry, description } => {
                let (value, option) = self.editable_option(*entry)?;
                let mut replacement = value.clone();
                replacement
                    .bytes
                    .splice(option.description_bytes, description_bytes(description)?);
                (Variable::Entry(*entry), Some(replacement))
            }
            Change::Remove { entry } => return self.remove(*entry),
            Change::Attach { entry } => {
                let (_, option) = self.editable_option(*entry)?;
                if !option.boot_category() {
                    return Err("只能将启动类别项添加到启动顺序。".into());
                }
                let mut order = self.order()?;
                if order.len() == MAX_ENTRIES || order.contains(entry) {
                    return Err("启动顺序已满或已包含此启动项。".into());
                }
                order.push(*entry);
                (Variable::Order, Some(boot_value(encode_ids(&order))))
            }
            Change::Create { .. } | Change::Restore | Change::Retarget { .. } => {
                return Err("缺少已验证的启动项来源。".into())
            }
            Change::Setup { next } => {
                let old = self
                    .setup_indications()
                    .ok_or("固件未提供进入设置界面的启动请求。")?;
                let updated = if *next {
                    old | BOOT_TO_FIRMWARE_UI
                } else {
                    old & !BOOT_TO_FIRMWARE_UI
                };
                if old == updated {
                    return Err("固件启动设置没有变化。".into());
                }
                (
                    Variable::Indications,
                    Some(boot_value(updated.to_le_bytes().to_vec())),
                )
            }
        };
        if self.writable(variable)? == replacement.as_ref() {
            return Err("固件启动设置没有变化。".into());
        }
        if replacement
            .as_ref()
            .is_some_and(|value| value.bytes.len() > MAX_VARIABLE_SIZE)
        {
            return Err("固件启动变量超过支持的大小。".into());
        }
        let mut after = self.clone();
        if let Some(value) = replacement {
            after.values.insert(variable, value);
        } else {
            after.values.remove(&variable);
        }
        let changes_active_order = match change {
            Change::Order { .. } => true,
            Change::Active {
                entry,
                active: false,
            } => after.order()?.contains(entry),
            _ => false,
        };
        if changes_active_order && !after.has_active_boot_entry()? {
            return Err("启动顺序必须保留至少一个已启用的有效启动项。".into());
        }
        Plan::new(self, after, vec![variable])
    }

    pub(super) fn check_revision(&self, revision: &str) -> Result<(), String> {
        if revision != self.revision()? {
            return Err(STALE.into());
        }
        Ok(())
    }

    pub(super) fn backup_entry(&self, entry: u16) -> Result<&Value, String> {
        let (value, _) = self.editable_option(entry)?;
        Ok(value)
    }

    pub(super) fn add(
        self,
        value: Value,
        mut read: impl FnMut(Variable) -> Result<Option<Value>, String>,
    ) -> Result<Plan, String> {
        validate_entry(&value)?;
        self.writable(Variable::Order)?;
        let mut order = self.order()?;
        if order.len() == MAX_ENTRIES {
            return Err("固件启动顺序已达到支持的数量上限。".into());
        }
        if self
            .values
            .iter()
            .any(|(key, old)| matches!(key, Variable::Entry(_)) && old == &value)
        {
            return Err("已有内容相同的固件启动项。".into());
        }
        let id = self.free_entry(&mut read)?;
        let key = Variable::Entry(id);
        order.push(id);
        let mut after = self.clone();
        after.values.insert(key, value);
        after
            .values
            .insert(Variable::Order, boot_value(encode_ids(&order)));
        Plan::new(self, after, vec![key, Variable::Order])
    }

    fn free_entry(
        &self,
        read: &mut impl FnMut(Variable) -> Result<Option<Value>, String>,
    ) -> Result<u16, String> {
        if self.complete_catalog
            && self
                .values
                .keys()
                .filter(|key| matches!(key, Variable::Entry(_)))
                .count()
                >= MAX_DISCOVERED
        {
            return Err("固件启动项数量已达支持上限。".into());
        }
        let referenced = self.referenced()?;
        for id in (0..=u16::MAX).filter(|id| !referenced.contains(id)) {
            let key = Variable::Entry(id);
            if !self.values.contains_key(&key) && read(key)?.is_none() {
                return Ok(id);
            }
        }
        Err("固件没有可用的启动项编号。".into())
    }

    pub(super) fn winpe_once(
        mut self,
        revision: &str,
        path: &[u8],
        mut read: impl FnMut(Variable) -> Result<Option<Value>, String>,
    ) -> Result<(Plan, u16, bool), String> {
        self.check_revision(revision)?;
        self.writable(Variable::Next)?;
        if self.selected(Variable::Next)?.is_some() {
            return Err("已有一次性固件启动项。请先清除该设置。".into());
        }
        if let Some(indications) = self.writable(Variable::Indications)? {
            let flags = u64::from_le_bytes(
                indications
                    .bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| "无法确认下次固件启动状态。")?,
            );
            if flags != 0 {
                return Err("已有其他固件启动请求。请完成该请求后再重启到 WinPE。".into());
            }
        }
        if !self.has_active_boot_entry()? {
            return Err("启动顺序中缺少可用的常规启动项。".into());
        }
        let matches_path = |value: &Value| {
            let Some(option) = LoadOption::parse(&value.bytes) else {
                return false;
            };
            option.active()
                && option.boot_category()
                && value.bytes.get(option.device_path.clone()) == Some(path)
                && option.device_path.end == value.bytes.len()
        };
        let mut existing = self.values.iter().find_map(|(key, value)| {
            let Variable::Entry(id) = key else {
                return None;
            };
            matches_path(value).then_some(*id)
        });
        if existing.is_none() && !self.complete_catalog {
            // A previous handoff entry is outside BootOrder and may have
            // disappeared from the referenced snapshot after BootNext cleared.
            for id in 0..MAX_DISCOVERED as u16 {
                let key = Variable::Entry(id);
                if self.values.contains_key(&key) {
                    continue;
                }
                if let Some(value) = read(key)?.filter(&matches_path) {
                    self.values.insert(key, value);
                    existing = Some(id);
                    break;
                }
            }
        }
        let mut after = self.clone();
        let mut steps = Vec::with_capacity(2);
        let id = if let Some(id) = existing {
            id
        } else {
            let id = self.free_entry(&mut read)?;
            let key = Variable::Entry(id);
            after
                .values
                .insert(key, new_load_option("DiskStudio WinPE", path)?);
            steps.push(key);
            id
        };
        after
            .values
            .insert(Variable::Next, boot_value(encode_ids(&[id])));
        steps.push(Variable::Next);
        Ok((Plan::new(self, after, steps)?, id, existing.is_none()))
    }

    fn remove(self, id: u16) -> Result<Plan, String> {
        let current = self
            .selected(Variable::Current)?
            .ok_or("当前固件启动项不可用。")?;
        if id == current || self.selected(Variable::Next)? == Some(id) {
            return Err("不能删除当前或已安排一次性启动的项。".into());
        }
        let key = Variable::Entry(id);
        self.writable(key)?.ok_or("所选固件启动项不存在。")?;
        let mut order = self.order()?;
        let mut after = self.clone();
        let steps = if order.contains(&id) {
            self.writable(Variable::Order)?;
            order.retain(|entry| *entry != id);
            after
                .values
                .insert(Variable::Order, boot_value(encode_ids(&order)));
            if !after.has_active_boot_entry()? {
                return Err("启动顺序必须保留至少一个已启用的有效启动项。".into());
            }
            vec![Variable::Order, key]
        } else {
            vec![key]
        };
        after.values.remove(&key);
        Plan::new(self, after, steps)
    }

    pub(super) fn retarget(
        self,
        entry: u16,
        path: &[u8],
        preserve_parameters: bool,
    ) -> Result<Plan, String> {
        let (original, option) = self.editable_option(entry)?;
        if !option.boot_category() || self.selected(Variable::Next)? == Some(entry) {
            return Err("只能改绑未安排为一次性启动的启动类别项。".into());
        }
        let order = self.order()?;
        if option.active()
            && order.contains(&entry)
            && !order
                .iter()
                .any(|id| *id != entry && self.active_boot_entry(*id))
        {
            return Err("改绑前需要在启动顺序中保留另一项已启用的有效启动项。".into());
        }
        let optional = option.device_path.end..original.bytes.len();
        let mut bytes = original.bytes[..option.device_path.start].to_vec();
        bytes[4..6].copy_from_slice(&path_length(path)?);
        bytes.extend(path);
        if preserve_parameters {
            bytes.extend(&original.bytes[optional]);
        }
        let replacement = boot_value(bytes);
        validate_entry(&replacement)?;
        if &replacement == original {
            return Err("固件启动设置没有变化。".into());
        }
        let key = Variable::Entry(entry);
        let mut after = self.clone();
        after.values.insert(key, replacement);
        Plan::new(self, after, vec![key])
    }
}

pub(super) struct Plan {
    pub(super) before: Snapshot,
    pub(super) after: Snapshot,
    steps: Vec<Variable>,
}

impl Plan {
    fn new(before: Snapshot, after: Snapshot, steps: Vec<Variable>) -> Result<Self, String> {
        let entry_bytes: usize = after
            .values
            .iter()
            .filter(|(key, _)| matches!(key, Variable::Entry(_)))
            .map(|(_, value)| value.bytes.len())
            .sum();
        if entry_bytes > MAX_CAPTURE_BYTES {
            return Err("更改后的固件启动项数据超过读取上限。".into());
        }
        Ok(Self {
            before,
            after,
            steps,
        })
    }

    #[cfg(test)]
    pub(super) fn apply(
        self,
        read: impl FnMut(Variable) -> Result<Option<Value>, String>,
        write: impl FnMut(Variable, Option<&Value>) -> Result<(), String>,
    ) -> Result<Report, String> {
        self.apply_catalog(read, write, || Ok(None))
    }

    pub(super) fn apply_catalog(
        self,
        mut read: impl FnMut(Variable) -> Result<Option<Value>, String>,
        mut write: impl FnMut(Variable, Option<&Value>) -> Result<(), String>,
        mut enumerate: impl FnMut() -> Result<Option<BTreeSet<u16>>, String>,
    ) -> Result<Report, String> {
        let watch = self.verify_current(&mut read, &mut enumerate)?;
        let mut expected = self.before;
        let mut completed = Vec::with_capacity(self.steps.len());
        for key in self.steps {
            let replacement = self.after.values.get(&key);
            if let Err(error) = write(key, replacement) {
                return if completed.is_empty() {
                    Err(error)
                } else {
                    Err(format!(
                        "已写入并核对 {}。更新 {} 失败。{error}",
                        completed.join(", "),
                        key.name()
                    ))
                };
            }
            if let Some(value) = replacement {
                expected.values.insert(key, value.clone());
            } else {
                expected.values.remove(&key);
            }
            let readback =
                Snapshot::capture_watching(&mut read, &mut enumerate, &watch).map_err(|error| {
                    format!("固件已接受写入 {}，但无法回读启动设置。{error}", key.name())
                })?;
            if readback != expected {
                return Err(format!(
                    "固件已接受写入 {}，但回读的启动设置与确认内容不同。",
                    key.name()
                ));
            }
            completed.push(key.name());
        }
        expected.report()
    }

    pub(super) fn verify_current(
        &self,
        mut read: impl FnMut(Variable) -> Result<Option<Value>, String>,
        mut enumerate: impl FnMut() -> Result<Option<BTreeSet<u16>>, String>,
    ) -> Result<BTreeSet<u16>, String> {
        let mut watch = self.before.referenced()?;
        watch.extend(self.after.referenced()?);
        if Snapshot::capture_watching(&mut read, &mut enumerate, &watch)? != self.before {
            return Err(STALE.into());
        }
        Ok(watch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn option(flags: u32, label: &str) -> Value {
        let mut path = vec![4, 1, 42, 0];
        path.extend([0; 20]);
        path.extend(uuid::Uuid::from_u128(1).to_bytes_le());
        path.extend([2, 2]);
        let filename = encode_ids(
            &"\\EFI\\Boot\\bootx64.efi"
                .encode_utf16()
                .chain([0])
                .collect::<Vec<_>>(),
        );
        path.extend([4, 4]);
        path.extend(((filename.len() + 4) as u16).to_le_bytes());
        path.extend(filename);
        path.extend([0x7f, 0xff, 4, 0]);
        let mut bytes = flags.to_le_bytes().to_vec();
        bytes.extend((path.len() as u16).to_le_bytes());
        bytes.extend(label.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        bytes.extend(path);
        bytes.extend([0x5a, 0x0f, 0xbe]);
        boot_value(bytes)
    }

    fn variables() -> BTreeMap<Variable, Value> {
        BTreeMap::from([
            (Variable::Order, boot_value(encode_ids(&[1, 2, 4]))),
            (
                Variable::Current,
                Value {
                    attributes: 6,
                    bytes: encode_ids(&[1]),
                },
            ),
            (Variable::Next, boot_value(encode_ids(&[3]))),
            (Variable::Entry(1), option(1, "Windows Boot Manager")),
            (Variable::Entry(2), option(9, "Secondary")),
            (Variable::Entry(3), option(1, "External")),
            (Variable::Entry(4), option(0, "Inactive")),
        ])
    }

    fn capture(values: &BTreeMap<Variable, Value>) -> Result<Snapshot, String> {
        Snapshot::capture(|key| Ok(values.get(&key).cloned()))
    }

    #[test]
    fn winpe_writes_only_its_entry_and_bootnext_and_reuses_the_entry_after_boot() {
        let mut initial = variables();
        initial.remove(&Variable::Next);
        let values = RefCell::new(initial);
        let read = |key| Ok(values.borrow().get(&key).cloned());
        let writes = RefCell::new(Vec::new());
        let write = |key, value: Option<&Value>| {
            writes.borrow_mut().push(key);
            values.borrow_mut().insert(key, value.unwrap().clone());
            Ok(())
        };
        let path = gpt_device_path(
            &["EFI".into(), "BOOT".into(), "BOOTX64.EFI".into()],
            1,
            uuid::Uuid::from_u128(28),
            2048,
            131072,
        )
        .unwrap();
        let before = capture(&values.borrow()).unwrap();
        let revision = before.revision().unwrap();
        let (plan, id, creates) = before.winpe_once(&revision, &path, read).unwrap();
        assert!(creates);
        let written = plan.apply(read, write).unwrap();
        assert_eq!(*writes.borrow(), [Variable::Entry(id), Variable::Next]);
        assert_eq!(written.order, [1, 2, 4]);
        assert_eq!(written.next, Some(id));
        let pending = capture(&values.borrow()).unwrap();
        let revision = pending.revision().unwrap();
        assert!(pending.winpe_once(&revision, &path, read).is_err());

        values.borrow_mut().remove(&Variable::Next);
        writes.borrow_mut().clear();
        let resumed = capture(&values.borrow()).unwrap();
        let revision = resumed.revision().unwrap();
        let (plan, reused, creates) = resumed.winpe_once(&revision, &path, read).unwrap();
        assert_eq!(reused, id);
        assert!(!creates);
        assert_eq!(plan.apply(read, write).unwrap().next, Some(id));
        assert_eq!(*writes.borrow(), [Variable::Next]);
    }

    fn prepare(values: &BTreeMap<Variable, Value>, change: Change) -> Result<Plan, String> {
        let snapshot = capture(values)?;
        let revision = snapshot.revision()?;
        snapshot.prepare(&revision, &change)
    }

    #[test]
    fn exposes_referenced_entries_in_firmware_order_and_checks_invalid_data() {
        let mut values = variables();
        values.remove(&Variable::Entry(2));
        values
            .get_mut(&Variable::Entry(4))
            .unwrap()
            .bytes
            .truncate(9);
        values.insert(Variable::Entry(55), option(1, "Unreferenced"));
        let report = capture(&values).unwrap().report().unwrap();
        assert_eq!(report.current, Some(1));
        assert_eq!(report.next, Some(3));
        assert_eq!(
            report.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            [1, 2, 4, 3]
        );
        assert_eq!(report.entries[0].gpt_partitions, [uuid::Uuid::from_u128(1)]);
        assert_eq!(report.entries[0].file_paths, ["\\EFI\\Boot\\bootx64.efi"]);
        assert!(matches!(report.entries[1].status, EntryStatus::Missing));
        assert!(matches!(report.entries[2].status, EntryStatus::Invalid));
        values.insert(Variable::Order, boot_value(encode_ids(&[1, 1])));
        assert!(capture(&values).is_err());
        values.insert(Variable::Order, boot_value(vec![0]));
        assert!(capture(&values).is_err());
    }

    #[test]
    fn decodes_additional_device_paths_and_rejects_broken_nodes() {
        let mut value = option(1, "Boot").bytes;
        let path_offset = 6 + "Boot".encode_utf16().count() * 2 + 2;
        let path_len = u16::from_le_bytes(value[4..6].try_into().unwrap());
        let next_path = [1, 1, 6, 0, 1, 1, 0x7f, 0xff, 4, 0];
        let next_offset = path_offset + usize::from(path_len);
        value.splice(next_offset..next_offset, next_path);
        value[4..6].copy_from_slice(&(path_len + 10).to_le_bytes());
        assert!(LoadOption::parse(&value).is_some());
        value[next_offset + 2] = 2;
        assert!(LoadOption::parse(&value).is_none());
    }

    #[test]
    fn rename_and_active_flag_preserve_device_path_and_optional_data() {
        let values = variables();
        let key = Variable::Entry(2);
        let old = &values[&key].bytes;
        let rename = prepare(
            &values,
            Change::Rename {
                entry: 2,
                description: "系统 🖥".into(),
            },
        )
        .unwrap();
        let new = &rename.after.values[&key].bytes;
        let before = LoadOption::parse(old).unwrap();
        let after = LoadOption::parse(new).unwrap();
        assert_eq!(after.description, "系统 🖥");
        assert_eq!(old[..6], new[..6]);
        assert_eq!(
            old[before.description_bytes.end..],
            new[after.description_bytes.end..]
        );
        let disable = prepare(
            &values,
            Change::Active {
                entry: 2,
                active: false,
            },
        )
        .unwrap();
        let new = &disable.after.values[&key].bytes;
        assert_eq!(u32::from_le_bytes(new[..4].try_into().unwrap()), 8);
        assert_eq!(old[4..], new[4..]);
        assert!(prepare(
            &values,
            Change::Rename {
                entry: 2,
                description: "old\0name".into()
            }
        )
        .is_err());
    }

    #[test]
    fn preserves_current_scheduled_and_last_valid_boot_entries() {
        let mut values = variables();
        for entry in [1, 3] {
            assert!(prepare(
                &values,
                Change::Active {
                    entry,
                    active: false
                }
            )
            .is_err());
        }
        assert!(prepare(&values, Change::Once { entry: 4 }).is_err());
        assert!(prepare(
            &values,
            Change::Order {
                entries: vec![1, 1, 4]
            }
        )
        .is_err());
        assert!(prepare(
            &values,
            Change::Order {
                entries: vec![1, 2]
            }
        )
        .is_err());
        assert!(prepare(
            &values,
            Change::Order {
                entries: vec![2, 4, 1]
            }
        )
        .is_ok());
        values.insert(Variable::Order, boot_value(encode_ids(&[2, 4])));
        assert!(prepare(
            &values,
            Change::Active {
                entry: 2,
                active: false
            }
        )
        .is_err());
        values.get_mut(&Variable::Next).unwrap().attributes |= 0x20;
        assert!(prepare(&values, Change::ClearOnce).is_err());
    }

    #[test]
    fn write_is_single_and_checks_stale_preview_and_readback() {
        let state = RefCell::new(variables());
        let stale = prepare(&state.borrow(), Change::Once { entry: 2 }).unwrap();
        state
            .borrow_mut()
            .insert(Variable::Entry(1), option(1, "Changed elsewhere"));
        assert_eq!(
            stale
                .apply(
                    |key| Ok(state.borrow().get(&key).cloned()),
                    |_, _| panic!("stale write")
                )
                .err()
                .as_deref(),
            Some(STALE)
        );
        let plan = prepare(&state.borrow(), Change::Once { entry: 2 }).unwrap();
        let writes = Cell::new(0);
        let report = plan
            .apply(
                |key| Ok(state.borrow().get(&key).cloned()),
                |key, value| {
                    writes.set(writes.get() + 1);
                    assert_eq!(key, Variable::Next);
                    state.borrow_mut().insert(key, value.unwrap().clone());
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(report.next, Some(2));
        assert_eq!(writes.get(), 1);
        assert_eq!(report.order, [1, 2, 4]);
        assert!(!report.entries.iter().any(|e| e.id == 3));
        let clear = prepare(&state.borrow(), Change::ClearOnce).unwrap();
        let report = clear
            .apply(
                |key| Ok(state.borrow().get(&key).cloned()),
                |key, value| {
                    assert_eq!(key, Variable::Next);
                    assert!(value.is_none());
                    state.borrow_mut().remove(&key);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(report.next, None);
        let plan = prepare(&state.borrow(), Change::Once { entry: 1 }).unwrap();
        assert!(plan
            .apply(|key| Ok(state.borrow().get(&key).cloned()), |_, _| Ok(()))
            .err()
            .unwrap()
            .contains("回读"));
    }

    #[test]
    fn add_checks_unlisted_slot_occupancy_and_reports_partial_failure() {
        let state = RefCell::new(variables());
        state
            .borrow_mut()
            .insert(Variable::Entry(0), option(1, "Unlisted"));
        let read = |key| Ok(state.borrow().get(&key).cloned());
        let value = option(1, "New");
        let plan = Snapshot::capture(read)
            .unwrap()
            .add(value.clone(), read)
            .unwrap();
        assert_eq!(plan.steps, [Variable::Entry(5), Variable::Order]);
        state
            .borrow_mut()
            .insert(Variable::Entry(5), option(1, "Added elsewhere"));
        assert_eq!(
            plan.apply(read, |_, _| panic!("occupied write"))
                .err()
                .as_deref(),
            Some(STALE)
        );
        state.borrow_mut().remove(&Variable::Entry(5));
        let plan = Snapshot::capture(read)
            .unwrap()
            .add(value.clone(), read)
            .unwrap();
        let error = plan
            .apply(read, |key, value| {
                if key == Variable::Order {
                    return Err("injected order failure".into());
                }
                assert_eq!(key, Variable::Entry(5));
                state.borrow_mut().insert(key, value.unwrap().clone());
                Ok(())
            })
            .err()
            .unwrap();
        assert!(error.contains("Boot0005") && error.contains("BootOrder"));
        assert_eq!(state.borrow()[&Variable::Entry(5)], value);
        assert_eq!(
            state.borrow()[&Variable::Order].bytes,
            encode_ids(&[1, 2, 4])
        );
    }

    #[test]
    fn remove_detaches_first_and_refuses_current_next_or_last_active() {
        let state = RefCell::new(variables());
        let read = |key| Ok(state.borrow().get(&key).cloned());
        assert!(prepare(&state.borrow(), Change::Remove { entry: 1 }).is_err());
        assert!(prepare(&state.borrow(), Change::Remove { entry: 3 }).is_err());
        let plan = prepare(&state.borrow(), Change::Remove { entry: 2 }).unwrap();
        let mut steps = Vec::new();
        let report = plan
            .apply(read, |key, value| {
                steps.push(key);
                if let Some(value) = value {
                    state.borrow_mut().insert(key, value.clone());
                } else {
                    assert_eq!(state.borrow()[&Variable::Order].bytes, encode_ids(&[1, 4]));
                    state.borrow_mut().remove(&key);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(steps, [Variable::Order, Variable::Entry(2)]);
        assert_eq!(report.order, [1, 4]);
        assert!(!state.borrow().contains_key(&Variable::Entry(2)));
        state.borrow_mut().insert(
            Variable::Current,
            Value {
                bytes: encode_ids(&[3]),
                attributes: 6,
            },
        );
        assert!(prepare(&state.borrow(), Change::Remove { entry: 1 }).is_err());
    }

    #[test]
    fn encodes_gpt_file_device_path_and_restores_complete_load_option() {
        let guid = uuid::Uuid::from_u128(0x65b7f123_1357_4ace_8765_567783341212);
        let path =
            gpt_device_path(&["EFI".into(), "启动.efi".into()], 16, guid, 2048, 623).unwrap();
        let value = new_load_option("Custom", &path).unwrap();
        let option = LoadOption::parse(&value.bytes).unwrap();
        assert_eq!(option.file_paths, ["\\EFI\\启动.efi"]);
        assert_eq!(option.gpt_partitions, [guid]);
        let disk_node = &value.bytes[option.description_bytes.end..][..42];
        assert_eq!(u32::from_le_bytes(disk_node[4..8].try_into().unwrap()), 16);
        assert_eq!(
            u64::from_le_bytes(disk_node[8..16].try_into().unwrap()),
            2048
        );
        assert_eq!(
            u64::from_le_bytes(disk_node[16..24].try_into().unwrap()),
            623
        );
        let original = variables()[&Variable::Entry(2)].clone();
        let mut backup = super::super::archive::encode(&original).unwrap();
        assert_eq!(super::super::archive::decode(&backup).unwrap(), original);
        let plan = capture(&variables())
            .unwrap()
            .add(value.clone(), |_| Ok(None))
            .unwrap();
        assert_eq!(plan.after.report().unwrap().order, [1, 2, 4, 0]);
        backup[16] ^= 1;
        assert!(super::super::archive::decode(&backup).is_err());
    }

    #[test]
    fn setup_request_changes_only_its_advertised_os_indications_bit() {
        let mut state = variables();
        assert!(prepare(&state, Change::Setup { next: true }).is_err());
        state.insert(
            Variable::IndicationsSupported,
            Value {
                attributes: 6,
                bytes: 0x41u64.to_le_bytes().to_vec(),
            },
        );
        state.insert(
            Variable::Indications,
            boot_value(0x40u64.to_le_bytes().to_vec()),
        );
        let plan = prepare(&state, Change::Setup { next: true }).unwrap();
        assert_eq!(plan.steps, [Variable::Indications]);
        assert_eq!(
            plan.after.values[&Variable::Indications].bytes,
            0x41u64.to_le_bytes()
        );
        assert_eq!(plan.after.report().unwrap().setup_next, Some(true));
        let reverse = prepare(&plan.after.values, Change::Setup { next: false }).unwrap();
        assert_eq!(
            reverse.after.values[&Variable::Indications],
            state[&Variable::Indications]
        );
        assert_eq!(
            reverse.after.values[&Variable::Next],
            state[&Variable::Next]
        );
    }

    #[test]
    fn unlisted_entries_can_be_attached_or_removed_with_exact_catalog_revalidation() {
        let state = RefCell::new(variables());
        state
            .borrow_mut()
            .insert(Variable::Entry(42), option(1, "Unlisted"));
        state
            .borrow_mut()
            .insert(Variable::Entry(70), boot_value(vec![1, 2, 3]));
        let read = |key| Ok(state.borrow().get(&key).cloned());
        let names = || {
            Ok(Some(
                state
                    .borrow()
                    .keys()
                    .filter_map(|key| {
                        if let Variable::Entry(id) = key {
                            Some(*id)
                        } else {
                            None
                        }
                    })
                    .collect::<BTreeSet<_>>(),
            ))
        };
        let snap = Snapshot::capture_catalog(read, names).unwrap();
        let report = snap.report().unwrap();
        assert!(report.all_entries);
        assert_eq!(
            report
                .entries
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            [1, 2, 4, 3, 42, 70]
        );
        assert!(matches!(report.entries[5].status, EntryStatus::Invalid));
        let add = snap
            .clone()
            .prepare(&report.revision, &Change::Attach { entry: 42 })
            .unwrap();
        state
            .borrow_mut()
            .insert(Variable::Entry(42), option(1, "Edited externally"));
        assert_eq!(
            add.apply_catalog(read, |_, _| panic!("stale write"), names)
                .err()
                .as_deref(),
            Some(STALE)
        );
        state
            .borrow_mut()
            .insert(Variable::Entry(42), option(1, "Unlisted"));
        let delete = snap
            .clone()
            .prepare(&report.revision, &Change::Remove { entry: 70 })
            .unwrap();
        let result = delete
            .apply_catalog(
                read,
                |key, value| {
                    assert_eq!(key, Variable::Entry(70));
                    assert!(value.is_none());
                    state.borrow_mut().remove(&key);
                    Ok(())
                },
                names,
            )
            .unwrap();
        assert_eq!(result.order, [1, 2, 4]);
        assert!(!result.entries.iter().any(|entry| entry.id == 70));
        let snap = Snapshot::capture_catalog(read, names).unwrap();
        let add = snap
            .prepare(&result.revision, &Change::Attach { entry: 42 })
            .unwrap();
        let result = add
            .apply_catalog(
                read,
                |key, value| {
                    assert_eq!(key, Variable::Order);
                    assert_eq!(value.unwrap().bytes, encode_ids(&[1, 2, 4, 42]));
                    state.borrow_mut().insert(key, value.unwrap().clone());
                    Ok(())
                },
                names,
            )
            .unwrap();
        assert_eq!(result.order, [1, 2, 4, 42]);
        assert_eq!(state.borrow()[&Variable::Entry(42)], option(1, "Unlisted"));
    }

    #[test]
    fn changing_catalog_membership_or_missing_values_is_not_a_complete_snapshot() {
        let values = variables();
        let read = |key| Ok(values.get(&key).cloned());
        let count = Cell::new(0);
        let unstable = || {
            count.set(count.get() + 1);
            let entries = if count.get() == 1 {
                [1, 2, 3, 4]
            } else {
                [0, 1, 2, 3]
            };
            Ok(Some(BTreeSet::from(entries)))
        };
        assert_eq!(
            Snapshot::capture_catalog(read, unstable).err().as_deref(),
            Some(STALE)
        );
        let missing = || Ok(Some(BTreeSet::from([1, 2, 3, 4, 5])));
        assert_eq!(
            Snapshot::capture_catalog(read, missing).err().as_deref(),
            Some(STALE)
        );
        assert!(!capture(&values).unwrap().report().unwrap().all_entries);
    }

    #[test]
    fn rebind_replaces_only_the_device_path_and_explicitly_keeps_or_drops_loader_parameters() {
        let values = variables();
        let path = gpt_device_path(
            &["EFI".into(), "new.efi".into()],
            5,
            uuid::Uuid::from_u128(2),
            512,
            256,
        )
        .unwrap();
        let snap = capture(&values).unwrap();
        let original = &values[&Variable::Entry(2)];
        let previous = LoadOption::parse(&original.bytes).unwrap();
        for preserve in [false, true] {
            let changed = snap.clone().retarget(2, &path, preserve).unwrap();
            assert_eq!(changed.steps, [Variable::Entry(2)]);
            assert_eq!(changed.after.order().unwrap(), [1, 2, 4]);
            let entry = &changed.after.values[&Variable::Entry(2)];
            let result = LoadOption::parse(&entry.bytes).unwrap();
            assert_eq!(result.description, previous.description);
            assert_eq!(result.flags, previous.flags);
            assert_eq!(result.file_paths, ["\\EFI\\new.efi"]);
            assert_eq!(result.gpt_partitions, [uuid::Uuid::from_u128(2)]);
            let expected = if preserve {
                &original.bytes[previous.device_path.end..]
            } else {
                &[]
            };
            assert_eq!(&entry.bytes[result.device_path.end..], expected);
            assert_eq!(
                changed.after.report().unwrap().entries[1].parameters_bytes,
                Some(expected.len())
            );
        }
        assert!(snap.retarget(3, &path, false).is_err());
        let mut only = values;
        only.insert(Variable::Order, boot_value(encode_ids(&[2])));
        assert!(capture(&only).unwrap().retarget(2, &path, false).is_err());
    }
}
