//! Interpret a retained Windows BCD store without opening the live registry.
use regf::{hive::RegistryKey, DataType, RegistryHive};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use uuid::Uuid;

#[path = "boot_configuration_rebuild.rs"]
pub(crate) mod rebuild;
#[path = "boot_configuration_recovery.rs"]
pub(crate) mod recovery;
#[path = "boot_configuration_repair.rs"]
pub(crate) mod repair;

pub(crate) const MAX_STORE: usize = 4 * 1024 * 1024;
const BOOT_MANAGER: Uuid = Uuid::from_u128(0x9dea862c_5cdd_4e70_acc1_f32b344d4795);
const MAX_OBJECTS: u32 = 512;
const OS_LOADER: u32 = 3;
const APPLICATION_TYPE: u32 = 0x000f_ffff;
const MAX_INHERITANCE_DEPTH: usize = 32;
const MAX_INHERITANCE_LINKS: usize = 4096;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub id: Uuid,
    pub object_type: u32,
    description: Option<String>,
    locale: Option<String>,
    pub path: Option<String>,
    pub system_root: Option<String>,
    #[serde(skip)]
    pub device: Option<Device>,
    #[serde(skip)]
    pub os_device: Option<Device>,
    recovery_sequence: Vec<Uuid>,
    recovery_enabled: Option<bool>,
    inherited_settings: Vec<InheritedSetting>,
    setting_issues: Vec<SettingIssue>,
    inheritance_issue: Option<InheritanceIssue>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Store {
    pub manager: Option<Entry>,
    default: Option<Uuid>,
    display_order: Vec<Uuid>,
    boot_sequence: Vec<Uuid>,
    tools_order: Vec<Uuid>,
    timeout_seconds: Option<String>,
    unresolved_objects: Vec<Uuid>,
    pub entries: Vec<Entry>,
}
impl Store {
    pub(crate) fn next_loader(&self) -> Option<&Entry> {
        self.manager.as_ref()?;
        let id = self.boot_sequence.first().copied().or(self.default)?;
        self.entries.iter().find(|entry| entry.id == id && entry.object_type & APPLICATION_TYPE == OS_LOADER)
    }
}

pub(crate) fn verify_winpe(bytes: Vec<u8>, bios: bool) -> Result<(), String> {
    let hive = clean_hive(bytes)?;
    let store = parse_hive(&hive)?;
    let entry = store.next_loader().ok_or("WinPE BCD 没有可核对的默认启动项。")?;
    let loader = if bios { "winload.exe" } else { "winload.efi" };
    if !entry.path.as_deref().is_some_and(|path| {
        path.eq_ignore_ascii_case(&format!(r"\windows\system32\boot\{loader}"))
            || path.eq_ignore_ascii_case(&format!(r"\windows\system32\{loader}"))
    }) || !entry.system_root.as_deref().is_some_and(|root| root.eq_ignore_ascii_case(r"\windows")) {
        return Err("WinPE BCD 的加载器或系统目录与选定的启动方式不符。".into());
    }
    let elements = hive.open_key(&format!(r"Objects\{{{}}}\Elements", entry.id)).map_err(|err|err.to_string())?;
    let image_path: Vec<_> = r"\sources\boot.wim".encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
    let options = |setting| -> Result<Uuid, String> {
        let value = element(Some(&elements), setting, DataType::Binary, 64*1024)?
            .ok_or("WinPE BCD 缺少 RAM 磁盘设备。")?;
        // The optical BCD stores a RAM disk device followed by its nested
        // boot-volume descriptor and the terminated, media-relative WIM path.
        let nested_boot = value.len().checked_sub(image_path.len() + 72)
            .and_then(|at| value.get(at..at + 72));
        let is_boot_volume = nested_boot.is_some_and(|descriptor| {
            let mut bcd_device = vec![0; 16];
            bcd_device.extend_from_slice(descriptor);
            decode_device(&bcd_device) == Device::Boot
        });
        if !matches!(decode_device(&value), Device::Unresolved {device_type:0})
            || value.get(32..36) != Some(&[3,0,0,0])
            || !value.get(value.len().saturating_sub(image_path.len())..)
                .is_some_and(|part| part.eq_ignore_ascii_case(&image_path))
            || !is_boot_volume
        {
            return Err("WinPE BCD 未引用介质中的 sources\\boot.wim。".into());
        }
        let id = Uuid::from_bytes_le(value[..16].try_into().unwrap());
        if id.is_nil() { return Err("WinPE BCD 缺少 RAM 磁盘选项标识。".into()); }
        Ok(id)
    };
    let ramdisk = options("11000001")?;
    if options("21000001")? != ramdisk {
        return Err("WinPE BCD 的加载器与系统设备引用不同。".into());
    }
    let sdi = hive.open_key(&format!(r"Objects\{{{ramdisk}}}\Elements")).map_err(|err|err.to_string())?;
    if !text(Some(&sdi),"32000004")?.is_some_and(|path| path.eq_ignore_ascii_case(r"\boot\boot.sdi"))
        || device(Some(&sdi),"31000003")? != Some(Device::Boot)
    {
        return Err("WinPE BCD 未通过本地 boot\\boot.sdi 引用检查。".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum Setting {
    Description,
    Locale,
    ApplicationDevice,
    ApplicationPath,
    SystemDevice,
    SystemRoot,
    RecoverySequence,
    RecoveryEnabled,
    Default,
    DisplayOrder,
    BootSequence,
    ToolsOrder,
    Timeout,
}

impl Setting {
    fn code(self) -> &'static str {
        match self {
            Self::Description => "12000004",
            Self::Locale => "12000005",
            Self::ApplicationDevice => "11000001",
            Self::ApplicationPath => "12000002",
            Self::SystemDevice => "21000001",
            Self::SystemRoot => "22000002",
            Self::RecoverySequence => "14000008",
            Self::RecoveryEnabled => "16000009",
            Self::Default => "23000003",
            Self::DisplayOrder => "24000001",
            Self::BootSequence => "24000002",
            Self::ToolsOrder => "24000010",
            Self::Timeout => "25000004",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InheritedSetting {
    field: Setting,
    object: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum SettingProblem {
    Invalid,
    Conflict,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingIssue {
    field: Setting,
    problem: SettingProblem,
    object: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum InheritanceProblem {
    MalformedList,
    MissingObject,
    IncompatibleObject,
    Cycle,
    Limit,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct InheritanceIssue {
    problem: InheritanceProblem,
    object: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Device {
    Boot,
    Gpt {
        disk: Uuid,
        partition: Uuid,
    },
    Mbr {
        signature: u32,
        #[serde(serialize_with = "decimal_offset")]
        offset: u64,
    },
    Unresolved {
        device_type: u32,
    },
    Invalid,
}

fn decimal_offset<S: serde::Serializer>(offset: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(offset)
}

fn key<'a>(parent: &RegistryKey<'a>, name: &str) -> Result<Option<RegistryKey<'a>>, String> {
    match parent.open_subkey(name) {
        Ok(value) => Ok(Some(value)),
        Err(regf::Error::KeyNotFound(_)) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn element(
    elements: Option<&RegistryKey<'_>>,
    number: &str,
    ty: DataType,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, String> {
    let Some(elements) = elements else {
        return Ok(None);
    };
    let Some(field) = key(elements, number)? else {
        return Ok(None);
    };
    let value = field.value("Element").map_err(|err| err.to_string())?;
    if value.data_type() != ty || value.data_size() as usize > max_bytes {
        return Err(format!("BCD {number}: unexpected element type or size"));
    }
    Ok(Some(value.raw_data().map_err(|err| err.to_string())?))
}

fn utf16(raw: &[u8]) -> Result<Vec<u16>, String> {
    if raw.len() % 2 != 0 || !raw.ends_with(&[0, 0]) {
        return Err("Malformed UTF-16 BCD element".into());
    }
    Ok(raw
        .chunks_exact(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .collect())
}

fn text(elements: Option<&RegistryKey<'_>>, number: &str) -> Result<Option<String>, String> {
    element(elements, number, DataType::String, 4096)?
        .map(|raw| {
            let units = utf16(&raw)?;
            let value =
                String::from_utf16(&units[..units.len() - 1]).map_err(|err| err.to_string())?;
            if value.chars().any(char::is_control) {
                return Err(format!("BCD {number}: control character in string"));
            }
            Ok(value)
        })
        .transpose()
}

fn object_list(
    elements: Option<&RegistryKey<'_>>,
    number: &str,
) -> Result<Option<Vec<Uuid>>, String> {
    let Some(raw) = element(
        elements,
        number,
        DataType::MultiString,
        80 * MAX_OBJECTS as usize + 2,
    )?
    else {
        return Ok(None);
    };
    let units = utf16(&raw)?;
    if units == [0] || units == [0, 0] {
        return Ok(Some(Vec::new()));
    }
    if !units.ends_with(&[0, 0]) {
        return Err(format!("BCD {number}: malformed object list terminator"));
    }
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    let names = &units[..units.len() - 2];
    for name in names.split(|&c| c == 0) {
        let name = String::from_utf16(name).map_err(|err| err.to_string())?;
        let id = Uuid::parse_str(&name).map_err(|err| err.to_string())?;
        if id.is_nil() || ids.len() >= MAX_OBJECTS as usize || !seen.insert(id) {
            return Err(format!(
                "BCD {number}: empty, repeated or too many object identifiers"
            ));
        }
        ids.push(id);
    }
    Ok(Some(ids))
}

fn object_id(elements: Option<&RegistryKey<'_>>, number: &str) -> Result<Option<Uuid>, String> {
    text(elements, number)?
        .map(|value| {
            Uuid::parse_str(&value)
                .ok()
                .filter(|id| !id.is_nil())
                .ok_or_else(|| format!("BCD {number}: malformed object identifier"))
        })
        .transpose()
}

fn seconds(elements: Option<&RegistryKey<'_>>, number: &str) -> Result<Option<String>, String> {
    element(elements, number, DataType::Binary, 8)?
        .map(|bytes| {
            if bytes.is_empty() {
                return Err(format!("BCD {number}: empty integer"));
            }
            let mut word = [0; 8];
            word[..bytes.len()].copy_from_slice(&bytes);
            Ok(u64::from_le_bytes(word).to_string())
        })
        .transpose()
}

fn boolean(elements: Option<&RegistryKey<'_>>, number: &str) -> Result<Option<bool>, String> {
    match element(elements, number, DataType::Binary, 2)?.as_deref() {
        None => Ok(None),
        Some([0] | [0, 0]) => Ok(Some(false)),
        Some([1] | [1, 0]) => Ok(Some(true)),
        _ => Err(format!("BCD {number}: malformed boolean")),
    }
}

fn device(elements: Option<&RegistryKey<'_>>, number: &str) -> Result<Option<Device>, String> {
    let Some(bytes) = element(elements, number, DataType::Binary, 64 * 1024)? else {
        return Ok(None);
    };
    Ok(Some(decode_device(&bytes)))
}

fn decode_device(bytes: &[u8]) -> Device {
    // A binary device value has an additional-options GUID followed by a
    // typed boot-library device descriptor. Qualified partition styles in
    // this descriptor are distinct from the WMI PartitionStyle enumeration.
    let (Some(options), Some(descriptor)) = (bytes.get(..16), bytes.get(16..)) else {
        return Device::Invalid;
    };
    if descriptor.len() < 16 {
        return Device::Invalid;
    }
    let word = |at: usize| u32::from_le_bytes(descriptor[at..at + 4].try_into().unwrap());
    let kind = word(0);
    if word(8) as usize != descriptor.len() {
        return Device::Invalid;
    }
    if !matches!(kind, 5 | 6) || word(4) != 0 || options.iter().any(|&byte| byte != 0) {
        return Device::Unresolved { device_type: kind };
    }
    if descriptor.len() != 72 || word(12) != 0 {
        return Device::Invalid;
    }
    if kind == 5 {
        return if descriptor[16..].iter().all(|&byte| byte == 0) {
            Device::Boot
        } else {
            Device::Invalid
        };
    }
    if word(32) != 0 || descriptor[56..].iter().any(|&byte| byte != 0) {
        return Device::Invalid;
    }
    match word(36) {
        0 => {
            let partition = Uuid::from_bytes_le(descriptor[16..32].try_into().unwrap());
            let disk = Uuid::from_bytes_le(descriptor[40..56].try_into().unwrap());
            if disk.is_nil() || partition.is_nil() {
                Device::Invalid
            } else {
                Device::Gpt { disk, partition }
            }
        }
        1 => {
            let offset = u64::from_le_bytes(descriptor[16..24].try_into().unwrap());
            let signature = word(40);
            if offset == 0
                || signature == 0
                || descriptor[24..32]
                    .iter()
                    .chain(&descriptor[44..56])
                    .any(|&byte| byte != 0)
            {
                Device::Invalid
            } else {
                Device::Mbr { signature, offset }
            }
        }
        _ => Device::Unresolved { device_type: kind },
    }
}

struct Object<'a> {
    kind: u32,
    elements: Option<RegistryKey<'a>>,
    parents: Result<Vec<Uuid>, String>,
}

fn inheritance_order(
    objects: &BTreeMap<Uuid, Object<'_>>,
    root: Uuid,
) -> Result<Vec<Uuid>, InheritanceIssue> {
    struct Walk<'a, 'h> {
        objects: &'a BTreeMap<Uuid, Object<'h>>,
        root: Uuid,
        application: u32,
        stack: HashSet<Uuid>,
        complete: HashSet<Uuid>,
        links: usize,
        order: Vec<Uuid>,
    }

    impl Walk<'_, '_> {
        fn visit(&mut self, id: Uuid) -> Result<(), InheritanceIssue> {
            use InheritanceProblem::*;
            let issue = |problem| InheritanceIssue {
                problem,
                object: id,
            };
            if self.complete.contains(&id) {
                return Ok(());
            }
            if self.stack.contains(&id) {
                return Err(issue(Cycle));
            }
            if self.stack.len() >= MAX_INHERITANCE_DEPTH {
                return Err(issue(Limit));
            }
            let node = self.objects.get(&id).ok_or_else(|| issue(MissingObject))?;
            if id != self.root
                && node.kind != 0x2010_0000
                && node.kind != (0x2020_0000 | self.application)
            {
                return Err(issue(IncompatibleObject));
            }
            let parents = node.parents.as_ref().map_err(|_| issue(MalformedList))?;
            self.links += parents.len();
            if self.links > MAX_INHERITANCE_LINKS {
                return Err(issue(Limit));
            }
            self.stack.insert(id);
            for &parent in parents {
                self.visit(parent)?;
            }
            self.stack.remove(&id);
            self.complete.insert(id);
            self.order.push(id);
            Ok(())
        }
    }

    let mut walk = Walk {
        objects,
        root,
        application: objects[&root].kind & APPLICATION_TYPE,
        stack: HashSet::new(),
        complete: HashSet::new(),
        links: 0,
        order: Vec::new(),
    };
    walk.visit(root)?;
    Ok(walk.order)
}

type Resolved<T> = Result<Option<(Uuid, T)>, (Uuid, SettingProblem)>;

fn inherited_value<T: Clone + PartialEq>(
    owner: Uuid,
    parents: &[Uuid],
    values: &HashMap<Uuid, Resolved<T>>,
) -> Resolved<T> {
    let mut selected: Option<&(Uuid, T)> = None;
    for parent in parents {
        let Some(candidate) = values[parent].as_ref().map_err(|issue| *issue)? else {
            continue;
        };
        if selected.is_some_and(|first| first.1 != candidate.1) {
            // Leave disagreeing branches unverified. The read-only inspector
            // does not mirror firmware or Windows precedence for conflicts.
            return Err((owner, SettingProblem::Conflict));
        }
        selected.get_or_insert(candidate);
    }
    Ok(selected.cloned())
}

struct Effective<'a, 'h> {
    objects: &'a BTreeMap<Uuid, Object<'h>>,
    root: Uuid,
    order: Result<Vec<Uuid>, InheritanceIssue>,
    inherited: Vec<InheritedSetting>,
    issues: Vec<SettingIssue>,
}

impl Effective<'_, '_> {
    fn get<T: Clone + PartialEq>(
        &mut self,
        field: Setting,
        read: impl Fn(Option<&RegistryKey<'_>>, &str) -> Result<Option<T>, String>,
    ) -> Option<T> {
        let code = field.code();
        let root = &self.objects[&self.root];
        let result = match read(root.elements.as_ref(), code) {
            Ok(Some(value)) => return Some(value),
            Err(_) => Err((self.root, SettingProblem::Invalid)),
            Ok(None) => {
                let order = self.order.as_ref().ok()?;
                let mut values = HashMap::new();
                for &id in &order[..order.len() - 1] {
                    let parent = &self.objects[&id];
                    // Library inheritance contains common elements. An
                    // application field belongs to the matching app class.
                    let local = if code.starts_with('2') && parent.kind == 0x2010_0000 {
                        Ok(None)
                    } else {
                        read(parent.elements.as_ref(), code)
                    };
                    let resolved = match local {
                        Ok(Some(value)) => Ok(Some((id, value))),
                        Ok(None) => inherited_value(id, parent.parents.as_ref().unwrap(), &values),
                        Err(_) => Err((id, SettingProblem::Invalid)),
                    };
                    values.insert(id, resolved);
                }
                inherited_value(self.root, root.parents.as_ref().unwrap(), &values)
            }
        };
        match result {
            Ok(Some((object, value))) => {
                self.inherited.push(InheritedSetting { field, object });
                Some(value)
            }
            Ok(None) => None,
            Err((object, problem)) => {
                self.issues.push(SettingIssue {
                    field,
                    problem,
                    object,
                });
                None
            }
        }
    }
}

fn clean_hive(bytes: Vec<u8>) -> Result<RegistryHive, String> {
    if !(4096..=MAX_STORE).contains(&bytes.len()) {
        return Err("BCD store length outside the inspection limit".into());
    }
    // Dirty hives require the matching transaction logs. Never publish stale
    // default or menu information from a hive with unequal sequence numbers.
    if &bytes[..4] != b"regf" || bytes[4..8] != bytes[8..12] {
        return Err("BCD signature or transaction state is invalid".into());
    }
    let hive = RegistryHive::from_bytes(bytes).map_err(|err| err.to_string())?;
    if !hive.base_block().checksum_valid {
        return Err("The BCD base block checksum is invalid".into());
    }
    Ok(hive)
}

pub(crate) fn parse(bytes: Vec<u8>) -> Result<Store, String> {
    parse_hive(&clean_hive(bytes)?)
}

/// Follow Windows Boot Manager's object references, including the menu,
/// one-time entries, inheritance, recovery, resume and RAM-disk options.
/// Firmware entries imported into the store by the current WinPE boot are
/// separate from the Windows boot chain unless that chain references them.
pub(crate) fn gpt_dependencies(bytes: Vec<u8>, disk: Uuid) -> Result<BTreeSet<Uuid>, String> {
    let hive = clean_hive(bytes)?;
    let objects = hive.open_key("Objects").map_err(|err| err.to_string())?;
    if objects.subkey_count() > MAX_OBJECTS {
        return Err("BCD object count exceeds the inspection limit".into());
    }
    let mut nodes = BTreeMap::new();
    for object in objects.subkeys().map_err(|err| err.to_string())? {
        let id = Uuid::parse_str(&object.name()).map_err(|err| err.to_string())?;
        if nodes.insert(id, object).is_some() {
            return Err("BCD 存在重复的启动配置对象。".into());
        }
    }
    if !nodes.contains_key(&BOOT_MANAGER) {
        return Err("Windows BCD 中没有可核对的下一次启动项。".into());
    }
    let mut pending = vec![BOOT_MANAGER];
    let mut visited = BTreeSet::from([BOOT_MANAGER]);
    let mut dependencies = BTreeSet::new();
    while let Some(id) = pending.pop() {
        // Optional menu and inheritance links can outlive their entries.
        // A missing object contributes no device to this retained store.
        let Some(object) = nodes.get(&id) else {
            continue;
        };
        let Some(elements) = key(object, "Elements")? else {
            continue;
        };
        let mut links = Vec::new();
        for field in elements.subkeys().map_err(|err| err.to_string())? {
            let name = field.name();
            let code = u32::from_str_radix(&name, 16).ok().filter(|_| name.len() == 8)
                .ok_or("BCD 存在无法识别的元素编号。")?;
            // BcdElementType encodes the data format in bits 24–27.
            match (code >> 24) & 0xf {
                1 => {
                    let bytes = element(Some(&elements), &name, DataType::Binary, 64 * 1024)?
                        .ok_or("BCD 缺少设备元素。")?;
                    let device = match decode_device(&bytes) {
                        Device::Unresolved { device_type: 0 } => {
                            let (parent, options) = ramdisk_parent(&bytes)
                                .ok_or("BCD 存在无法绑定到分区的设备，请迁移全部分区。")?;
                            links.push(options);
                            parent
                        }
                        device => device,
                    };
                    match device {
                        Device::Gpt { disk: owner, partition } if owner == disk => {
                            dependencies.insert(partition);
                        }
                        Device::Boot | Device::Gpt { .. } | Device::Mbr { .. } => {}
                        Device::Unresolved { .. } | Device::Invalid => {
                            return Err("BCD 存在无法绑定到分区的设备，请迁移全部分区。".into());
                        }
                    }
                }
                3 => links.extend(object_id(Some(&elements), &name)?),
                4 => {
                    if let Some(ids) = object_list(Some(&elements), &name)? {
                        links.extend(ids);
                    }
                }
                _ => {}
            }
        }
        pending.extend(links.into_iter().filter(|id| visited.insert(*id)));
    }
    Ok(dependencies)
}

// Accept the checked Windows RAM-disk file descriptor with a single direct
// boot-volume or qualified partition parent and a terminated UTF-16 path.
fn ramdisk_parent(raw: &[u8]) -> Option<(Device, Uuid)> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(raw.get(at..at + 4)?.try_into().ok()?))
    };
    let options = Uuid::from_bytes_le(raw.get(..16)?.try_into().ok()?);
    if options.is_nil()
        || word(16)? != 0
        || !matches!(word(20)?, 0 | 1)
        || word(24)? as usize != raw.len().checked_sub(16)?
        || word(28)? != 0
        || word(32)? != 3
        || word(56)? != 1
        || word(60)? as usize != raw.len().checked_sub(56)?
        || word(64)? != 5
    {
        return None;
    }
    let path = utf16(raw.get(140..)?).ok()?;
    let path = String::from_utf16(path.get(..path.len().checked_sub(1)?)?).ok()?;
    if !path.starts_with('\\') || path.chars().any(char::is_control) {
        return None;
    }
    let mut parent = [0; 88];
    parent[16..].copy_from_slice(raw.get(68..140)?);
    let device = decode_device(&parent);
    matches!(device, Device::Boot | Device::Gpt { .. } | Device::Mbr { .. }).then_some((device, options))
}

fn parse_hive(hive: &RegistryHive) -> Result<Store, String> {
    let objects = hive.open_key("Objects").map_err(|err| err.to_string())?;
    if objects.subkey_count() > MAX_OBJECTS {
        return Err("BCD object count exceeds the inspection limit".into());
    }
    let mut nodes = BTreeMap::new();
    for object in objects.subkeys().map_err(|err| err.to_string())? {
        let id = Uuid::parse_str(&object.name()).map_err(|err| err.to_string())?;
        if id.is_nil() || nodes.contains_key(&id) {
            return Err("BCD contains an empty or duplicate object identifier".into());
        }
        let kind = object
            .open_subkey("Description")
            .and_then(|description| description.value("Type")?.dword_data())
            .map_err(|err| err.to_string())?;
        if id == BOOT_MANAGER && (kind >> 28 != 1 || kind & APPLICATION_TYPE != 2) {
            return Err("BCD manager identifier has the wrong application type".into());
        }
        let elements = if matches!(kind >> 28, 1 | 2) {
            key(&object, "Elements")?
        } else {
            None
        };
        let parents = object_list(elements.as_ref(), "14000006").map(Option::unwrap_or_default);
        nodes.insert(
            id,
            Object {
                kind,
                elements,
                parents,
            },
        );
    }

    let mut applications = BTreeMap::new();
    let mut default = None;
    let mut display_order = Vec::new();
    let mut boot_sequence = Vec::new();
    let mut tools_order = Vec::new();
    let mut timeout_seconds = None;
    for (&id, object) in &nodes {
        if object.kind >> 28 != 1 {
            continue;
        }
        let mut effective = Effective {
            objects: &nodes,
            root: id,
            order: inheritance_order(&nodes, id),
            inherited: Vec::new(),
            issues: Vec::new(),
        };
        if id == BOOT_MANAGER {
            default = effective.get(Setting::Default, object_id);
            display_order = effective
                .get(Setting::DisplayOrder, object_list)
                .unwrap_or_default();
            boot_sequence = effective
                .get(Setting::BootSequence, object_list)
                .unwrap_or_default();
            tools_order = effective
                .get(Setting::ToolsOrder, object_list)
                .unwrap_or_default();
            timeout_seconds = effective.get(Setting::Timeout, seconds);
        }
        let os_loader = object.kind & APPLICATION_TYPE == OS_LOADER;
        applications.insert(
            id,
            Entry {
                id,
                object_type: object.kind,
                description: effective.get(Setting::Description, text),
                locale: effective.get(Setting::Locale, text),
                path: effective.get(Setting::ApplicationPath, text),
                system_root: os_loader
                    .then(|| effective.get(Setting::SystemRoot, text))
                    .flatten(),
                device: effective.get(Setting::ApplicationDevice, device),
                os_device: os_loader
                    .then(|| effective.get(Setting::SystemDevice, device))
                    .flatten(),
                recovery_sequence: effective
                    .get(Setting::RecoverySequence, object_list)
                    .unwrap_or_default(),
                recovery_enabled: effective.get(Setting::RecoveryEnabled, boolean),
                inherited_settings: effective.inherited,
                setting_issues: effective.issues,
                inheritance_issue: effective.order.err(),
            },
        );
    }
    let manager = applications.remove(&BOOT_MANAGER);
    let mut unresolved_objects: Vec<_> = default
        .iter()
        .chain(&display_order)
        .chain(&boot_sequence)
        .chain(&tools_order)
        .chain(
            manager
                .iter()
                .chain(applications.values())
                .flat_map(|entry| &entry.recovery_sequence),
        )
        .copied()
        .filter(|id| !applications.contains_key(id))
        .collect();
    unresolved_objects.sort_unstable();
    unresolved_objects.dedup();
    let mut entries = Vec::new();
    for id in &display_order {
        entries.extend(applications.remove(id));
    }
    entries.extend(applications.into_values());
    Ok(Store {
        manager,
        default,
        display_order,
        boot_sequence,
        tools_order,
        timeout_seconds,
        unresolved_objects,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regf::HiveBuilder;

    const GPT_DISK: Uuid = Uuid::from_u128(0x92fafa75_cc8a_456b_b082_16c499fff0c1);
    const GPT_PARTITION: Uuid = Uuid::from_u128(0xbb1f4714_4af2_47c8_96eb_6f162b1b57ad);

    fn object(hive: &mut HiveBuilder, id: Uuid, ty: u32) -> String {
        let path = format!("Objects\\{{{id}}}");
        let description = format!("{path}\\Description");
        hive.create_key(&description).unwrap();
        hive.add_value_dword(&description, Some("Type"), ty)
            .unwrap();
        path
    }

    fn list(hive: &mut HiveBuilder, object: &str, number: &str, ids: &[Uuid]) {
        let path = format!("{object}\\Elements\\{number}");
        let mut raw = Vec::new();
        for id in ids {
            raw.extend(
                format!("{{{id}}}\0")
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            );
        }
        raw.extend([0, 0]);
        hive.create_key(&path).unwrap();
        hive.add_value_with_type(&path, Some("Element"), &raw, DataType::MultiString)
            .unwrap();
    }

    fn field(hive: &mut HiveBuilder, object: &str, number: &str) -> String {
        let path = format!("{object}\\Elements\\{number}");
        hive.create_key(&path).unwrap();
        path
    }

    fn descriptor(kind: u32) -> [u8; 88] {
        let mut bytes = [0; 88];
        bytes[16..20].copy_from_slice(&kind.to_le_bytes());
        bytes[24..28].copy_from_slice(&72u32.to_le_bytes());
        bytes
    }

    fn gpt_device() -> [u8; 88] {
        let mut bytes = descriptor(6);
        bytes[32..48].copy_from_slice(&GPT_PARTITION.to_bytes_le());
        bytes[56..72].copy_from_slice(&GPT_DISK.to_bytes_le());
        bytes
    }

    #[test]
    fn selection_checks_an_sdi_object_and_a_ramdisk_parent() {
        let mut hive = HiveBuilder::new().unwrap();
        let manager = object(&mut hive, BOOT_MANAGER, 0x1010_0002);
        let recovery_id = Uuid::from_u128(0xbcd1);
        let recovery = object(&mut hive, recovery_id, 0x1020_0003);
        list(&mut hive, &manager, "24000001", &[recovery_id, Uuid::from_u128(0xbcd7)]);
        let sdi_id = Uuid::from_u128(0xbcd2);
        let sdi = object(&mut hive, sdi_id, 0x3000_0000);
        let sdi_partition = Uuid::from_u128(0xbcd3);
        let inner = gpt_device();
        let path: Vec<_> = r"\Recovery\WindowsRE\Winre.wim".encode_utf16()
            .chain([0]).flat_map(u16::to_le_bytes).collect();
        let mut ramdisk = vec![0; 140];
        ramdisk.extend(path);
        let size = ramdisk.len() as u32;
        ramdisk[..16].copy_from_slice(&sdi_id.to_bytes_le());
        for (at, word) in [(20, 1u32), (24, size - 16), (32, 3), (56, 1), (60, size - 56), (64, 5)] {
            ramdisk[at..at + 4].copy_from_slice(&word.to_le_bytes());
        }
        ramdisk[68..140].copy_from_slice(&inner[16..]);
        let device = field(&mut hive, &recovery, "21000001");
        hive.add_value_binary(&device, Some("Element"), &ramdisk).unwrap();
        let mut sdi_device = inner;
        sdi_device[32..48].copy_from_slice(&sdi_partition.to_bytes_le());
        let device = field(&mut hive, &sdi, "31000003");
        hive.add_value_binary(&device, Some("Element"), &sdi_device).unwrap();
        let firmware = object(&mut hive, Uuid::from_u128(0xbcd5), 0x101f_ffff);
        let device = field(&mut hive, &firmware, "11000001");
        sdi_device[32..48].copy_from_slice(&Uuid::from_u128(0xbcd6).to_bytes_le());
        hive.add_value_binary(&device, Some("Element"), &sdi_device).unwrap();
        assert_eq!(gpt_dependencies(hive.build().unwrap(), GPT_DISK).unwrap(),
            BTreeSet::from([GPT_PARTITION, sdi_partition]));
    }

    #[test]
    fn selection_refuses_an_unresolved_resume_device() {
        let mut hive = HiveBuilder::new().unwrap();
        let manager = object(&mut hive, BOOT_MANAGER, 0x1010_0002);
        let resume_id = Uuid::from_u128(0xbcd4);
        let resume = object(&mut hive, resume_id, 0x1020_0004);
        list(&mut hive, &manager, "24000001", &[resume_id]);
        let device = field(&mut hive, &resume, "21000001");
        hive.add_value_binary(&device, Some("Element"), &descriptor(8)).unwrap();
        assert!(gpt_dependencies(hive.build().unwrap(), GPT_DISK).is_err());
    }

    fn fixture() -> Vec<u8> {
        let mut hive = HiveBuilder::new().unwrap();
        let first = "{7619dcc9-fafe-11d9-b411-000476eba25f}";
        let missing = "{338e9515-0ba9-43b3-b155-19d3438cd166}";
        let manager = format!("Objects\\{{{BOOT_MANAGER}}}");
        let loader = format!("Objects\\{first}");
        for (path, ty) in [(&manager, 0x10100002), (&loader, 0x10200003)] {
            let path = format!("{path}\\Description");
            hive.create_key(&path).unwrap();
            hive.add_value_dword(&path, Some("Type"), ty).unwrap();
        }
        let description = format!("{loader}\\Elements\\12000004");
        let configuration = format!("{manager}\\Elements");
        let default = format!("{configuration}\\23000003");
        let order = format!("{configuration}\\24000001");
        let timeout = format!("{configuration}\\25000004");
        for path in [&description, &default, &order, &timeout] {
            hive.create_key(path).unwrap();
        }
        hive.add_value_string(&description, Some("Element"), "Windows Setup")
            .unwrap();
        hive.add_value_string(&default, Some("Element"), first)
            .unwrap();
        let multi: Vec<_> = format!("{first}\0{missing}\0\0")
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        hive.add_value_with_type(&order, Some("Element"), &multi, DataType::MultiString)
            .unwrap();
        hive.add_value_binary(&timeout, Some("Element"), &[30])
            .unwrap();
        let device = format!("{loader}\\Elements\\11000001");
        let system = format!("{loader}\\Elements\\21000001");
        hive.create_key(&device).unwrap();
        hive.create_key(&system).unwrap();
        hive.add_value_binary(&device, Some("Element"), &gpt_device())
            .unwrap();
        hive.add_value_binary(&system, Some("Element"), &descriptor(5))
            .unwrap();
        hive.build().unwrap()
    }

    #[test]
    fn uses_default_and_menu_order_without_treating_missing_targets_as_valid() {
        let store = parse(fixture()).unwrap();
        assert_eq!(store.manager.unwrap().id, BOOT_MANAGER);
        assert_eq!(store.timeout_seconds.as_deref(), Some("30"));
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.default, Some(store.entries[0].id));
        assert_eq!(
            store.display_order,
            [store.entries[0].id, store.unresolved_objects[0]]
        );
        assert_eq!(store.unresolved_objects.len(), 1);
        assert_eq!(
            store.entries[0].device,
            Some(Device::Gpt {
                disk: GPT_DISK,
                partition: GPT_PARTITION
            })
        );
        assert_eq!(store.entries[0].os_device, Some(Device::Boot));
        assert_eq!(
            store.entries[0].description.as_deref(),
            Some("Windows Setup")
        );
    }

    #[test]
    fn checks_qualified_mbr_byte_offset_and_leaves_indirect_devices_unresolved() {
        let mut bytes = descriptor(6);
        let offset = 0x8_0010_0000u64;
        bytes[32..40].copy_from_slice(&offset.to_le_bytes());
        bytes[52..56].copy_from_slice(&1u32.to_le_bytes());
        bytes[56..60].copy_from_slice(&0x1e7dd4f4u32.to_le_bytes());
        assert_eq!(
            decode_device(&bytes),
            Device::Mbr {
                signature: 0x1e7dd4f4,
                offset
            }
        );
        assert_eq!(decode_device(&bytes[..87]), Device::Invalid);
        bytes[0] = 1;
        assert_eq!(decode_device(&bytes), Device::Unresolved { device_type: 6 });
        bytes[0] = 0;
        bytes[16] = 0;
        assert_eq!(decode_device(&bytes), Device::Unresolved { device_type: 0 });
        bytes[16] = 6;
        bytes[56..60].fill(0);
        assert_eq!(decode_device(&bytes), Device::Invalid);
    }

    #[test]
    fn resolves_nested_typed_inheritance_and_keeps_direct_values() {
        let mut hive = HiveBuilder::new().unwrap();
        let loader_id = Uuid::from_u128(1);
        let app_id = Uuid::from_u128(2);
        let common_id = Uuid::from_u128(3);
        let shared_id = Uuid::from_u128(4);
        let recovery_id = Uuid::from_u128(5);
        let loader = object(&mut hive, loader_id, 0x1020_0003);
        let app = object(&mut hive, app_id, 0x2020_0003);
        let common = object(&mut hive, common_id, 0x2010_0000);
        let shared = object(&mut hive, shared_id, 0x2010_0000);
        let manager = object(&mut hive, BOOT_MANAGER, 0x1010_0002);
        let _ = object(&mut hive, recovery_id, 0x1020_0003);
        list(&mut hive, &loader, "14000006", &[app_id, shared_id]);
        list(&mut hive, &app, "14000006", &[common_id]);
        list(&mut hive, &shared, "14000006", &[common_id]);
        list(&mut hive, &manager, "24000002", &[recovery_id]);
        list(&mut hive, &manager, "24000010", &[]);
        list(&mut hive, &loader, "14000008", &[recovery_id]);
        for (node, number, value) in [
            (&common, "12000005", "zh-CN"),
            (&common, "12000002", r"\Example\winload.efi"),
            (&app, "22000002", r"\Windows"),
            (&common, "22000002", r"\Incompatible"),
            (&loader, "12000002", r"\Windows\System32\winload.efi"),
        ] {
            let key = field(&mut hive, node, number);
            hive.add_value_string(&key, Some("Element"), value).unwrap();
        }
        let key = field(&mut hive, &app, "21000001");
        hive.add_value_binary(&key, Some("Element"), &gpt_device())
            .unwrap();
        let key = field(&mut hive, &common, "16000009");
        hive.add_value_binary(&key, Some("Element"), &[0, 0])
            .unwrap();
        let store = parse(hive.build().unwrap()).unwrap();
        assert_eq!(store.boot_sequence, [recovery_id]);
        assert!(store.tools_order.is_empty());
        assert!(store.unresolved_objects.is_empty());
        let loader = &store.entries[0];
        assert_eq!(loader.id, loader_id);
        assert_eq!(loader.locale.as_deref(), Some("zh-CN"));
        assert_eq!(
            loader.path.as_deref(),
            Some(r"\Windows\System32\winload.efi")
        );
        assert_eq!(loader.system_root.as_deref(), Some(r"\Windows"));
        assert!(matches!(loader.os_device, Some(Device::Gpt { .. })));
        assert_eq!(loader.recovery_enabled, Some(false));
        assert_eq!(loader.recovery_sequence, [recovery_id]);
        assert!(loader.inheritance_issue.is_none());
        assert!(loader.setting_issues.is_empty());
        assert!(loader
            .inherited_settings
            .iter()
            .any(|source| source.field == Setting::SystemRoot && source.object == app_id));
        assert!(!loader
            .inherited_settings
            .iter()
            .any(|source| source.field == Setting::ApplicationPath));
    }

    #[test]
    fn keeps_valid_direct_settings_when_inheritance_is_broken_or_conflicting() {
        for (parent_type, inherited_cycle, expected) in [
            (None, false, Some(InheritanceProblem::MissingObject)),
            (
                Some(0x2020_0004),
                false,
                Some(InheritanceProblem::IncompatibleObject),
            ),
            (Some(0x2010_0000), true, Some(InheritanceProblem::Cycle)),
            (Some(0x2010_0000), false, None),
        ] {
            let mut hive = HiveBuilder::new().unwrap();
            let (loader_id, first_id, second_id) =
                (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
            let loader = object(&mut hive, loader_id, 0x1020_0003);
            let second = object(&mut hive, second_id, 0x2010_0000);
            list(&mut hive, &loader, "14000006", &[first_id, second_id]);
            let key = field(&mut hive, &loader, "12000002");
            hive.add_value_string(&key, Some("Element"), r"\winload.efi")
                .unwrap();
            let key = field(&mut hive, &second, "12000005");
            hive.add_value_string(&key, Some("Element"), "en-US")
                .unwrap();
            if let Some(ty) = parent_type {
                let first = object(&mut hive, first_id, ty);
                let key = field(&mut hive, &first, "12000005");
                hive.add_value_string(&key, Some("Element"), "zh-CN")
                    .unwrap();
                if inherited_cycle {
                    list(&mut hive, &first, "14000006", &[first_id]);
                }
            }
            let store = parse(hive.build().unwrap()).unwrap();
            let loader = &store.entries[0];
            assert_eq!(
                loader.inheritance_issue.map(|issue| issue.problem),
                expected
            );
            assert_eq!(loader.path.as_deref(), Some(r"\winload.efi"));
            assert!(loader.locale.is_none());
            if expected.is_none() {
                assert_eq!(loader.setting_issues[0].field, Setting::Locale);
                assert_eq!(loader.setting_issues[0].problem, SettingProblem::Conflict);
            }
        }
    }

    #[test]
    fn refuses_to_interpret_a_store_that_requires_log_replay() {
        let mut bytes = fixture();
        bytes[4] ^= 1;
        assert!(parse(bytes).err().unwrap().contains("transaction state"));
    }
}
