use quick_xml::{events::Event, Reader};
use serde::Serialize;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};
use uuid::Uuid;

const MAX_DESCRIPTOR: usize = 256 * 1024;
const MAX_FILES: usize = 64;
const MAX_LAYERS: usize = 16;
const MAX_SECTORS: u64 = 1 << 34;
const DEFAULT_TOP: Uuid = Uuid::from_u128(0x5fbaabe3_6958_40ff_92a7_860e329aab41);
const BACKUP_ID: Uuid = Uuid::from_u128(0x704718e1_2314_44c8_9087_d78ed36b0f4e);
const XML_ERROR: &str = "Parallels 描述文件的 XML 无效或超出解析上限。";
const STRUCTURE_ERROR: &str = "Parallels 描述文件缺少必要字段或字段重复。";
const GRAPH_ERROR: &str = "Parallels 快照标识或父子关系不完整、重复或存在循环。";
const GEOMETRY_ERROR: &str = "Parallels 分卷范围有间隙、重叠或与声明容量不符。";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Descriptor {
    pub(crate) size: u64,
    pub(crate) top: Uuid,
    pub(crate) snapshots: Vec<Snapshot>,
    storages: Vec<Storage>,
}

#[derive(Serialize)]
pub(crate) struct Snapshot {
    pub(crate) id: Uuid,
    parent: Option<Uuid>,
    pub(crate) name: String,
    pub(crate) supported: bool,
}

pub(crate) fn progress_identity(files: String, snapshot: Uuid) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"DiskStudio Parallels historical snapshot v1\0");
    digest.update(files.as_bytes());
    digest.update(snapshot.as_bytes());
    format!("{:x}", digest.finalize())
}

#[derive(Serialize)]
struct Storage {
    start: u64,
    end: u64,
    cluster: u64,
    layers: Vec<Layer>,
}

#[derive(Serialize)]
struct Layer {
    id: Uuid,
    name: String,
    sparse: bool,
}

#[derive(Default)]
struct Node {
    name: String,
    version: Option<String>,
    text: String,
    children: Vec<Self>,
}

impl Node {
    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Self> {
        self.children.iter().filter(move |node| node.name == name)
    }

    fn optional<'a>(&'a self, name: &'a str) -> Result<Option<&'a Self>, &'static str> {
        let mut nodes = self.children(name);
        let first = nodes.next();
        if nodes.next().is_some() {
            return Err(STRUCTURE_ERROR);
        }
        Ok(first)
    }

    fn required<'a>(&'a self, name: &'a str) -> Result<&'a Self, &'static str> {
        self.optional(name)?.ok_or(STRUCTURE_ERROR)
    }

    fn value(&self) -> Result<&str, &'static str> {
        if !self.children.is_empty() {
            return Err(STRUCTURE_ERROR);
        }
        Ok(self.text.trim())
    }

    fn number(&self, name: &str) -> Result<u64, &'static str> {
        let value = self.required(name)?.value()?;
        if !value.bytes().all(|c| c.is_ascii_digit()) {
            return Err(GEOMETRY_ERROR);
        }
        value.parse().map_err(|_| GEOMETRY_ERROR)
    }

    fn guid(&self) -> Result<Uuid, &'static str> {
        let value = self.value()?;
        if value.len() != 38 || !value.starts_with('{') || !value.ends_with('}') {
            return Err(GRAPH_ERROR);
        }
        Uuid::parse_str(value).map_err(|_| GRAPH_ERROR)
    }
}

fn decode(bytes: &[u8]) -> Result<(Cow<'_, str>, Option<bool>), &'static str> {
    let little_endian = if bytes.starts_with(&[0xff, 0xfe]) {
        true
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        false
    } else {
        let text = std::str::from_utf8(bytes).map_err(|_| XML_ERROR)?;
        return Ok((Cow::Borrowed(text.trim_start_matches('\u{feff}')), None));
    };
    let mut units = bytes[2..].chunks_exact(2);
    if !units.remainder().is_empty() {
        return Err(XML_ERROR);
    }
    let text = char::decode_utf16(units.by_ref().map(|w| {
        let pair = [w[0], w[1]];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    }))
    .collect::<Result<String, _>>()
    .map_err(|_| XML_ERROR)?;
    Ok((Cow::Owned(text), Some(little_endian)))
}

fn attach(stack: &mut [Node], root: &mut Option<Node>, node: Node) -> Result<(), &'static str> {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if root.is_none() {
        *root = Some(node);
    } else {
        return Err(XML_ERROR);
    }
    Ok(())
}

fn parse_xml(bytes: &[u8]) -> Result<Node, &'static str> {
    if bytes.len() > MAX_DESCRIPTOR {
        return Err(XML_ERROR);
    }
    let (xml, encoding) = decode(bytes)?;
    if xml.chars().any(|c| !valid_char(c)) {
        return Err(XML_ERROR);
    }
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().check_comments = true;
    let mut stack: Vec<Node> = Vec::new();
    let (mut root, mut declared) = (None, false);
    for _ in 0..16384 {
        let event = reader.read_event().map_err(|_| XML_ERROR)?;
        match &event {
            Event::Start(tag) | Event::Empty(tag) => {
                if stack.len() >= 24 || tag.name().as_ref().contains(':') {
                    return Err(XML_ERROR);
                }
                let mut node = Node {
                    name: tag.name().as_ref().to_owned(),
                    ..Node::default()
                };
                for attr in tag.attributes() {
                    let attr = attr.map_err(|_| XML_ERROR)?;
                    if attr.key.as_ref() == "xmlns" || attr.key.as_ref().contains(':') {
                        return Err(XML_ERROR);
                    }
                    if attr.key.as_ref() == "Version" && stack.is_empty() {
                        let value = attr
                            .normalized_value(quick_xml::XmlVersion::default())
                            .map_err(|_| XML_ERROR)?;
                        node.version = Some(value.into_owned());
                    }
                }
                if matches!(event, Event::Start(_)) {
                    stack.push(node);
                } else {
                    attach(&mut stack, &mut root, node)?;
                }
            }
            Event::End(_) => {
                let node = stack.pop().ok_or(XML_ERROR)?;
                attach(&mut stack, &mut root, node)?;
            }
            Event::Text(text) => {
                let text = text.xml10_content();
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&text);
                } else if !text.trim().is_empty() {
                    return Err(XML_ERROR);
                }
            }
            Event::CData(text) => stack
                .last_mut()
                .ok_or(XML_ERROR)?
                .text
                .push_str(&text.xml10_content()),
            Event::GeneralRef(reference) => {
                let ch = match reference.resolve_char_ref().map_err(|_| XML_ERROR)? {
                    Some(ch) => ch,
                    None => match reference.as_ref() {
                        "amp" => '&',
                        "lt" => '<',
                        "gt" => '>',
                        "apos" => '\'',
                        "quot" => '"',
                        _ => return Err(XML_ERROR),
                    },
                };
                if !valid_char(ch) {
                    return Err(XML_ERROR);
                }
                stack.last_mut().ok_or(XML_ERROR)?.text.push(ch);
            }
            Event::Decl(decl) => {
                if declared
                    || root.is_some()
                    || !stack.is_empty()
                    || decl.version().map_err(|_| XML_ERROR)?.as_ref() != "1.0"
                {
                    return Err(XML_ERROR);
                }
                if let Some(value) = decl.encoding() {
                    let value = value.map_err(|_| XML_ERROR)?;
                    let valid = match encoding {
                        None => value.eq_ignore_ascii_case("utf-8"),
                        Some(le) => {
                            value.eq_ignore_ascii_case("utf-16")
                                || value.eq_ignore_ascii_case(if le {
                                    "utf-16le"
                                } else {
                                    "utf-16be"
                                })
                        }
                    };
                    if !valid {
                        return Err(XML_ERROR);
                    }
                }
                declared = true;
            }
            Event::Comment(_) => {}
            Event::PI(_) | Event::DocType(_) => return Err(XML_ERROR),
            Event::Eof => {
                return if stack.is_empty() {
                    root.ok_or(XML_ERROR)
                } else {
                    Err(XML_ERROR)
                }
            }
        }
    }
    Err(XML_ERROR)
}

fn valid_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

// A descriptor path supplies only a display/matching name. The parser never
// opens files, and Windows, macOS and relative directory prefixes are ignored.
fn file_name(value: &str) -> Result<&str, &'static str> {
    let name = value.rsplit(['/', '\\']).next().unwrap_or_default();
    if name.is_empty()
        || name.encode_utf16().count() > 255
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || ":*?\"<>|".contains(c))
        || name.eq_ignore_ascii_case("DiskDescriptor.xml")
    {
        return Err("Parallels 描述文件中的数据文件名无效。");
    }
    Ok(name)
}

fn snapshot_graph(
    snapshots: &Node,
) -> Result<(Uuid, HashMap<Uuid, Uuid>, HashMap<Uuid, usize>), &'static str> {
    let top = snapshots
        .optional("TopGUID")?
        .map(Node::guid)
        .transpose()?
        .unwrap_or(DEFAULT_TOP);
    let mut parents = HashMap::new();
    let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for shot in snapshots.children("Shot") {
        let guid = shot.required("GUID")?.guid()?;
        let parent = shot.required("ParentGUID")?.guid()?;
        if guid.is_nil() || parents.insert(guid, parent).is_some() {
            return Err(GRAPH_ERROR);
        }
        children.entry(parent).or_default().push(guid);
    }
    let roots = children.get(&Uuid::nil()).ok_or(GRAPH_ERROR)?;
    if roots.len() != 1 || top == BACKUP_ID || !parents.contains_key(&top) {
        return Err(GRAPH_ERROR);
    }
    let mut depths = HashMap::new();
    let mut pending = vec![(roots[0], 1)];
    while let Some((guid, depth)) = pending.pop() {
        depths.insert(guid, depth);
        if let Some(nodes) = children.get(&guid) {
            pending.extend(nodes.iter().map(|id| (*id, depth + 1)));
        }
    }
    if depths.len() != parents.len() {
        return Err(GRAPH_ERROR);
    }
    Ok((top, parents, depths))
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Descriptor, &'static str> {
    let root = parse_xml(bytes)?;
    if root.name != "Parallels_disk_image" || root.version.as_deref().is_some_and(|v| v != "1.0") {
        return Err("Parallels 描述文件版本暂不支持。");
    }
    let parameters = root.required("Disk_Parameters")?;
    let sectors = parameters.number("Disk_size")?;
    if !(8192..=MAX_SECTORS).contains(&sectors) || parameters.number("Padding")? != 0 {
        return Err("Parallels 目录包容量或扇区填充方式暂不支持。");
    }
    let (top, parents, depths) = snapshot_graph(root.required("Snapshots")?)?;
    let mut extents = vec![];
    let mut names = HashSet::new();
    for storage in root.required("StorageData")?.children("Storage") {
        if extents.len() >= MAX_FILES - 1 {
            return Err("Parallels 目录包的数据分卷数量超出上限。");
        }
        let start = storage.number("Start")?;
        let end = storage.number("End")?;
        let cluster = storage.number("Blocksize")?;
        if start >= end || end > sectors || !(1..=524288).contains(&cluster) {
            return Err(GEOMETRY_ERROR);
        }
        let mut images = HashMap::new();
        for image in storage.children("Image") {
            let guid = image.required("GUID")?.guid()?;
            let parent = parents.get(&guid).ok_or(GRAPH_ERROR)?;
            let sparse = match image.required("Type")?.value()? {
                "Plain" if parent.is_nil() => false,
                "Compressed" => true,
                _ => return Err("Parallels 数据类型暂不支持，只有基盘可以使用平面数据。"),
            };
            if let Some(flags) = image.optional("Flags")? {
                if !matches!(flags.value()?, "" | "0") {
                    return Err("Parallels 数据文件含未知功能标志。");
                }
            }
            let name = file_name(image.required("File")?.value()?)?;
            if !names.insert(name.to_lowercase())
                || images
                    .insert(
                        guid,
                        Layer {
                            id: guid,
                            name: name.into(),
                            sparse,
                        },
                    )
                    .is_some()
            {
                return Err("Parallels 描述文件含重复的数据文件名或快照标识。");
            }
        }
        if images.len() != parents.len() {
            return Err(GRAPH_ERROR);
        }
        let mut layers: Vec<_> = images.into_values().collect();
        layers.sort_unstable_by_key(|layer| layer.id);
        extents.push(Storage {
            start: start * 512,
            end: end * 512,
            cluster: cluster * 512,
            layers,
        });
    }
    extents.sort_by_key(|s| s.start);
    let mut cursor = 0;
    for storage in &extents {
        if cursor != storage.start {
            return Err(GEOMETRY_ERROR);
        }
        cursor = storage.end;
    }
    let size = sectors * 512;
    if cursor != size {
        return Err(GEOMETRY_ERROR);
    }
    let mut snapshots: Vec<_> = extents[0]
        .layers
        .iter()
        .map(|layer| Snapshot {
            id: layer.id,
            parent: Some(parents[&layer.id]).filter(|id| !id.is_nil()),
            name: layer.name.clone(),
            supported: depths[&layer.id] <= MAX_LAYERS
                && depths[&layer.id] * extents.len() < MAX_FILES,
        })
        .collect();
    snapshots.sort_by(|a, b| (a.id != top, &a.name, a.id).cmp(&(b.id != top, &b.name, b.id)));
    Ok(Descriptor {
        size,
        top,
        snapshots,
        storages: extents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "{5f07cb09-ef78-422e-b000-111111111111}";
    const ACTIVE: &str = "{5fbaabe3-6958-40ff-92a7-860e329aab41}";
    const NIL: &str = "{00000000-0000-0000-0000-000000000000}";

    fn fixture() -> String {
        format!(
            r#"<Parallels_disk_image Version="1.0">
            <Disk_Parameters><Disk_size>12288</Disk_size><Padding>0</Padding></Disk_Parameters>
            <StorageData>
              <Storage><Start>4096</Start><End>12288</End><Blocksize>128</Blocksize>
                <Image><GUID>{ROOT}</GUID><Type>Compressed</Type><File>upper-base.hds</File></Image>
                <Image><GUID>{ACTIVE}</GUID><Type>Compressed</Type><Flags/><File>upper-child.hds</File></Image>
              </Storage>
              <Storage><Start>0</Start><End>4096</End><Blocksize>128</Blocksize>
                <Image><GUID>{ROOT}</GUID><Type>Plain</Type><File>C:\copies\base &amp; root.hdd</File></Image>
                <Image><GUID>{ACTIVE}</GUID><Type>Compressed</Type><File>folder/lower-child.hds</File></Image>
              </Storage>
            </StorageData>
            <Snapshots><Shot><GUID>{ROOT}</GUID><ParentGUID>{NIL}</ParentGUID></Shot>
                <Shot><GUID>{ACTIVE}</GUID><ParentGUID>{ROOT}</ParentGUID></Shot></Snapshots>
          </Parallels_disk_image>"#
        )
    }

    #[test]
    fn resolves_split_extents_and_fallback_top_without_resolving_paths() {
        let parsed = parse(fixture().as_bytes()).unwrap();
        assert_eq!(parsed.size, 6 * 1024 * 1024);
        let [first, second] = parsed.storages.as_slice() else {
            panic!("missing split extent")
        };
        assert_eq!(
            (first.start, first.end, first.cluster),
            (0, 2 * 1024 * 1024, 65536)
        );
        assert_eq!(parsed.top, Uuid::parse_str(ACTIVE).unwrap());
        assert_eq!(parsed.snapshots[0].name, "lower-child.hds");
        assert_eq!(
            parsed.snapshots[0].parent,
            Some(Uuid::parse_str(ROOT).unwrap())
        );
        assert_eq!(first.layers[1].name, "lower-child.hds");
        assert!(first.layers[1].sparse);
        assert_eq!(first.layers[0].name, "base & root.hdd");
        assert!(!first.layers[0].sparse);
        assert_eq!(second.start, first.end);
        assert_eq!(second.layers[0].name, "upper-base.hds");
    }

    #[test]
    fn explicit_top_uses_its_own_chain_and_accepts_utf16_descriptor() {
        let xml = "<?xml version=\"1.0\" encoding=\"utf-16\"?>".to_owned()
            + &fixture().replace(
                "<Snapshots>",
                &format!("<Snapshots><TopGUID>{ROOT}</TopGUID>"),
            );
        for little_endian in [false, true] {
            let mut data = if little_endian {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for unit in xml.encode_utf16() {
                data.extend(if little_endian {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            let parsed = parse(&data).unwrap();
            assert_eq!(parsed.top, Uuid::parse_str(ROOT).unwrap());
            assert!(parsed.snapshots[0].parent.is_none());
            assert!(parsed.storages.iter().all(|s| s.layers.len() == 2));
            data.pop();
            assert!(parse(&data).is_err());
        }
    }

    #[test]
    fn rejects_ambiguous_or_incomplete_xmldata_and_inconsistent_geometry() {
        let xml = fixture();
        for (a, b) in [
            ("<Padding>0", "<Padding>1"),
            ("<Start>4096", "<Start>4095"),
            ("<Start>4096", "<Start>4097"),
            ("<End>12288", "<End>12289"),
            ("<Blocksize>128", "<Blocksize>0"),
            (
                "<Disk_size>12288</Disk_size>",
                "<Disk_size>12288</Disk_size><Disk_size>12288</Disk_size>",
            ),
            ("<Flags/>", "<Flags>Unsupported</Flags>"),
            ("folder/lower-child.hds", "UPPER-CHILD.HDS"),
            ("folder/lower-child.hds", "folder/"),
            (
                "<Type>Compressed</Type><File>folder",
                "<Type>Plain</Type><File>folder",
            ),
            ("base &amp; root.hdd", "base &unknown; root.hdd"),
            ("<Snapshot", "<Missing"),
            ("Version=\"1.0\"", "Version=\"2.0\""),
        ] {
            let changed = xml.replacen(a, b, 1);
            assert_ne!(changed, xml);
            assert!(
                parse(changed.as_bytes()).is_err(),
                "accepted replacement {a} → {b}"
            );
        }
        assert!(parse(format!("<!DOCTYPE Parallels_disk_image>{xml}").as_bytes()).is_err());
        assert!(parse(format!("{xml}{xml}").as_bytes()).is_err());
        assert!(parse(&vec![b' '; MAX_DESCRIPTOR + 1]).is_err());
    }

    #[test]
    fn rejects_missing_or_cyclic_parents_and_the_wrong_top_id() {
        let xml = fixture();
        for parent in [NIL, ACTIVE, "{12345678-1234-1234-1234-123456789012}"] {
            let changed = xml.replace(
                &format!("<ParentGUID>{ROOT}</ParentGUID>"),
                &format!("<ParentGUID>{parent}</ParentGUID>"),
            );
            assert!(parse(changed.as_bytes()).is_err());
        }
        let bad_top = xml.replace(
            "<Snapshots>",
            &format!("<Snapshots><TopGUID>{NIL}</TopGUID>"),
        );
        assert!(parse(bad_top.as_bytes()).is_err());
    }

    #[test]
    fn historical_progress_bindings_distinguish_views_of_the_same_files() {
        let files = "a".repeat(64);
        let first = progress_identity(files.clone(), Uuid::parse_str(ROOT).unwrap());
        let second = progress_identity(files.clone(), Uuid::parse_str(ACTIVE).unwrap());
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
        assert_ne!(first, files);
    }
}
