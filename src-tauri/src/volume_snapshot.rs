//! Complete, bounded association snapshots for batch state checks. No cache
//! survives a read or mutation. Method targets remain original provider paths.
use super::{err, policy, Disk, Partition, Session, Volume, DISKS};
use serde::Deserialize;
use std::collections::BTreeMap;

const VOLUMES: &str =
    "SELECT __PATH,ObjectId,Path,FileSystem,FileSystemLabel,DriveLetter,Size FROM MSFT_Volume";
const LINKS: &str = "SELECT Partition,Volume FROM MSFT_PartitionToVolume";
const LIMIT: usize = 32768;
const NAMESPACE: &str = "ROOT\\Microsoft\\Windows\\Storage";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Link {
    partition: String,
    volume: String,
}

struct Scope(String);
impl Scope {
    // __PATH comes from the fixed query on the local WMI connection. Its
    // named server is the authority; a dot is an alias for that server only.
    fn from_disk(path: &str) -> Result<Self, String> {
        let (server, _) = split(path)?;
        if server == "." {
            return Err("磁盘提供程序未返回可核对的本机名称。".into());
        }
        let scope = Self(server.to_ascii_uppercase());
        scope.key(path, "MSFT_Disk")?;
        Ok(scope)
    }
    fn key(&self, path: &str, class: &str) -> Result<String, String> {
        let (server, tail) = split(path)?;
        if server != "." && !server.eq_ignore_ascii_case(&self.0) {
            return Err("卷关联引用了另一台计算机，已拒绝操作。".into());
        }
        let (namespace, object) = tail.split_once(':').ok_or("卷关联缺少命名空间")?;
        let prefix = format!("{class}.ObjectId=\"");
        if !namespace.eq_ignore_ascii_case(NAMESPACE)
            || !object
                .get(..prefix.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(&prefix))
        {
            return Err("卷关联的命名空间、类型或标识键不可确认。".into());
        }
        // Preserve the entire escaped key. Do not strip or decode arbitrary
        // host names, namespaces, suffixes or partial ObjectId matches.
        let value = &object[prefix.len()..];
        let mut chars = value.chars();
        let mut nonempty = false;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if !matches!(chars.next(), Some('\\' | '"')) {
                        return Err("卷关联标识的转义无效。".into());
                    }
                    nonempty = true;
                }
                '"' if nonempty && chars.next().is_none() => return Ok(object.to_ascii_uppercase()),
                '"' => break,
                _ => nonempty = true,
            }
        }
        Err("卷关联标识不完整。".into())
    }
}
fn split(path: &str) -> Result<(&str, &str), String> {
    if path.len() > 32768 || path.chars().any(char::is_control) {
        return Err("卷关联引用长度或字符无效。".into());
    }
    let (server, tail) = path
        .strip_prefix(r"\\")
        .and_then(|s| s.split_once('\\'))
        .ok_or("缺少完整的本地卷关联引用")?;
    if server.is_empty()
        || !server
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
    {
        return Err("卷关联的计算机名称无效。".into());
    }
    Ok((server, tail))
}

#[derive(PartialEq, Eq)]
pub(super) struct Snapshot {
    partitions: BTreeMap<String, Partition>,
    volumes: BTreeMap<String, Volume>,
    links: BTreeMap<String, Vec<String>>,
    reverse: BTreeMap<String, Vec<String>>,
}
impl Snapshot {
    fn new(
        scope: &Scope,
        partitions: &[Partition],
        volumes: Vec<Volume>,
        links: Vec<Link>,
    ) -> Result<Self, String> {
        if partitions.len() > LIMIT || volumes.len() > LIMIT || links.len() > LIMIT {
            return Err("卷关联清单超过安全上限。".into());
        }
        let mut result = Self {
            partitions: BTreeMap::new(),
            volumes: BTreeMap::new(),
            links: BTreeMap::new(),
            reverse: BTreeMap::new(),
        };
        for p in partitions {
            let key = scope.key(&p.path, "MSFT_Partition")?;
            if result.partitions.insert(key, p.clone()).is_some() {
                return Err("分区对象引用不唯一。".into());
            }
        }
        for v in volumes {
            let key = scope.key(&v.path, "MSFT_Volume")?;
            if result.volumes.insert(key, v).is_some() {
                return Err("卷对象引用不唯一。".into());
            }
        }
        for link in links {
            let p = scope.key(&link.partition, "MSFT_Partition")?;
            let v = scope.key(&link.volume, "MSFT_Volume")?;
            if !result.partitions.contains_key(&p) || !result.volumes.contains_key(&v) {
                return Err("卷关联包含清单中不存在的对象，请刷新后重新预览。".into());
            }
            result.links.entry(p.clone()).or_default().push(v.clone());
            result.reverse.entry(v).or_default().push(p);
        }
        for values in result.links.values_mut().chain(result.reverse.values_mut()) {
            values.sort(); // Keep duplicates: repeated association rows are ambiguous.
        }
        Ok(result)
    }
    pub(super) fn volume(&self, disk: &Disk, p: &Partition) -> Result<Option<Volume>, String> {
        let key = Scope::from_disk(&disk.path)?.key(&p.path, "MSFT_Partition")?;
        if self.partitions.get(&key) != Some(p) {
            return Err("分区布局或保护信息与卷关联快照不一致。".into());
        }
        let Some(forward) = self.links.get(&key) else {
            return Ok(None);
        };
        if forward.len() != 1
            || self.reverse.get(&forward[0]).map(Vec::as_slice) != Some(std::slice::from_ref(&key))
        {
            return Err("分区与卷的双向关联不唯一，已拒绝操作。".into());
        }
        let volume = self.volumes.get(&forward[0]).ok_or("卷关联目标已经消失")?;
        let letter = volume
            .drive_letter
            .as_deref()
            .map(policy::clean)
            .unwrap_or("");
        if !letter.is_empty() && (letter.len() != 1 || !letter.as_bytes()[0].is_ascii_alphabetic())
        {
            return Err("卷盘符不可确认。".into());
        }
        Ok(Some(volume.clone()))
    }
}

pub(super) fn read(session: &Session, disk: &Disk) -> Result<Snapshot, String> {
    let scope = Scope::from_disk(&disk.path)?;
    let volumes: Vec<Volume> = session.con.raw_query(VOLUMES).map_err(err)?;
    let links: Vec<Link> = session.con.raw_query(LINKS).map_err(err)?;
    let first = Snapshot::new(&scope, &session.partitions, volumes, links)?;
    // A full second read brackets the association lookup. It detects a layout,
    // protection, volume or association change during collection, without a
    // per-partition round trip or a reusable metadata cache.
    let disks: Vec<Disk> = session.con.raw_query(DISKS).map_err(err)?;
    policy::inventory(&disks)?;
    if disks.iter().find(|d| d.number == disk.number) != Some(disk) {
        return Err("读取卷关联时磁盘标识或保护信息已改变。".into());
    }
    let partitions: Vec<Partition> = session.con.raw_query(Partition::WMI_SELECT).map_err(err)?;
    let volumes: Vec<Volume> = session.con.raw_query(VOLUMES).map_err(err)?;
    let links: Vec<Link> = session.con.raw_query(LINKS).map_err(err)?;
    let second = Snapshot::new(&scope, &partitions, volumes, links)?;
    unchanged(first, second)
}
fn unchanged(first: Snapshot, second: Snapshot) -> Result<Snapshot, String> {
    if first != second {
        return Err("读取期间完整布局、保护信息或卷关联已改变，请重新预览。".into());
    }
    Ok(second)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn path(server: &str, class: &str, id: &str) -> String {
        format!(r#"\\{server}\{NAMESPACE}:{class}.ObjectId="{id}""#)
    }
    fn fixture() -> (Disk, Vec<Partition>, Vec<Volume>, Vec<Link>) {
        let disk = Disk {
            path: path("TEST-PC", "MSFT_Disk", "disk"),
            ..Default::default()
        };
        let parts = vec![Partition {
            path: path("TEST-PC", "MSFT_Partition", r"part\\one"),
            guid: Some("original".into()),
            is_hidden: Some(false),
            ..Default::default()
        }];
        let volumes = vec![Volume {
            path: path("TEST-PC", "MSFT_Volume", "volume"),
            object_id: "volume".into(),
            volume_path: Some(r"\\?\Volume{synthetic}\".into()),
            file_system: Some("NTFS".into()),
            file_system_label: Some("DATA".into()),
            drive_letter: None,
            size: Some(64 * 1048576),
        }];
        let links = vec![Link {
            partition: path(".", "MSFT_Partition", r"part\\one"),
            volume: path(".", "MSFT_Volume", "volume"),
        }];
        (disk, parts, volumes, links)
    }
    #[test]
    fn letterless_volume_uses_complete_provider_keys_and_local_dot_alias_only() {
        let (d, p, v, links) = fixture();
        let scope = Scope::from_disk(&d.path).unwrap();
        let snapshot = Snapshot::new(&scope, &p, v.clone(), links).unwrap();
        assert_eq!(snapshot.volume(&d, &p[0]).unwrap(), Some(v[0].clone()));
        let missing = Snapshot::new(&scope, &p, v, vec![]).unwrap();
        assert_eq!(missing.volume(&d, &p[0]).unwrap(), None);
    }
    #[test]
    fn rejects_foreign_hosts_classes_namespaces_incomplete_and_partial_keys() {
        let (d, p, v, links) = fixture();
        let scope = Scope::from_disk(&d.path).unwrap();
        for bad in [
            path("REMOTE", "MSFT_Partition", r"part\\one"),
            path(".", "MSFT_Volume", r"part\\one"),
            links[0].partition.replace(NAMESPACE, "ROOT\\CIMV2"),
            links[0].partition.replace("ObjectId", "OtherId"),
            format!("{}extra", links[0].partition),
            path(".", "MSFT_Partition", "part"),
            path(".", "MSFT_Partition", r"part\one"),
            "MSFT_Partition.ObjectId=\"part\"".into(),
        ] {
            let mut changed = links.clone();
            changed[0].partition = bad;
            assert!(Snapshot::new(&scope, &p, v.clone(), changed).is_err());
        }
    }
    #[test]
    fn rejects_duplicate_dangling_and_many_to_one_associations() {
        let (d, mut p, v, links) = fixture();
        let scope = Scope::from_disk(&d.path).unwrap();
        assert!(
            Snapshot::new(&scope, &p, vec![v[0].clone(), v[0].clone()], links.clone()).is_err()
        );
        assert!(Snapshot::new(&scope, &p, vec![], links.clone()).is_err());
        let doubled = Snapshot::new(
            &scope,
            &p,
            v.clone(),
            vec![links[0].clone(), links[0].clone()],
        )
        .unwrap();
        assert!(doubled.volume(&d, &p[0]).is_err());
        let mut other = p[0].clone();
        other.path = path("TEST-PC", "MSFT_Partition", "other");
        other.partition_number = 2;
        p.push(other);
        let mut added = links.clone();
        added.push(Link {
            partition: p[1].path.clone(),
            volume: links[0].volume.clone(),
        });
        let shared = Snapshot::new(&scope, &p, v, added).unwrap();
        assert!(shared.volume(&d, &p[0]).is_err());
        assert!(shared.volume(&d, &p[1]).is_err());
    }
    #[test]
    fn snapshot_changes_in_protection_layout_volume_and_links_fail_closed() {
        let (d, p, v, links) = fixture();
        let scope = Scope::from_disk(&d.path).unwrap();
        let fresh = || Snapshot::new(&scope, &p, v.clone(), links.clone()).unwrap();
        unchanged(fresh(), fresh()).unwrap();
        let mut changed = p.clone();
        changed[0].is_hidden = Some(true);
        assert!(unchanged(
            fresh(),
            Snapshot::new(&scope, &changed, v.clone(), links.clone()).unwrap()
        )
        .is_err());
        assert!(fresh().volume(&d, &changed[0]).is_err());
        changed[0] = p[0].clone();
        changed[0].size += 1048576;
        assert!(unchanged(
            fresh(),
            Snapshot::new(&scope, &changed, v.clone(), links.clone()).unwrap()
        )
        .is_err());
        let mut volumes = v.clone();
        volumes[0].file_system_label = Some("CHANGED".into());
        assert!(unchanged(
            fresh(),
            Snapshot::new(&scope, &p, volumes, links.clone()).unwrap()
        )
        .is_err());
        assert!(unchanged(fresh(), Snapshot::new(&scope, &p, v, vec![]).unwrap()).is_err());
    }
    #[test]
    fn oversized_snapshot_and_ambiguous_partition_paths_are_rejected() {
        let (d, p, v, links) = fixture();
        let scope = Scope::from_disk(&d.path).unwrap();
        assert!(Snapshot::new(&scope, &p, v.clone(), vec![links[0].clone(); LIMIT + 1]).is_err());
        assert!(Snapshot::new(&scope, &[p[0].clone(), p[0].clone()], v, links).is_err());
    }
}
