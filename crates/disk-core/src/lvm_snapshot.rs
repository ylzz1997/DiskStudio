//! Classic device-mapper persistent copy-on-write snapshots.
use super::*;

const BAD_RELATION: &str = "LVM2 写时复制快照的原卷或存储映射缺失、重复或不受支持。";
const BAD_STORE: &str = "LVM2 写时复制快照已失效或例外表不完整。";
const MERGING: &str = "LVM2 写时复制快照正在合并。";
const LIMIT: &str = "LVM2 写时复制快照例外表超出本次读取限制。";
const MISSING: &str = "LVM2 写时复制快照需要未选择的物理卷。";
const EXCEPTION_BYTES: usize = 16;
const MAX_EXCEPTIONS: usize = 4_000_000;

pub(super) struct Image {
    origin: Option<Arc<Volume>>,
    store: Arc<Volume>,
    exceptions: Vec<(u64, u64)>,
}

impl Image {
    pub(super) fn degraded_raid(&self) -> bool {
        self.store.degraded_raid
            || self
                .origin
                .as_ref()
                .is_some_and(|origin| origin.degraded_raid)
    }

    pub(super) fn location(&self, position: u64, chunk: u64) -> (Option<&Volume>, u64, u64) {
        let number = position / chunk;
        let slot = self.exceptions.partition_point(|&(old, _)| old < number);
        if let Some(&(old, new)) = self.exceptions.get(slot) {
            if old == number {
                let within = position % chunk;
                return (Some(&self.store), new * chunk + within, chunk - within);
            }
            return (self.origin.as_deref(), position, old * chunk - position);
        }
        (self.origin.as_deref(), position, u64::MAX)
    }
}

struct Link<'a> {
    internal: &'a str,
    spec: &'a Record,
}

pub(super) struct Catalog<'a> {
    stores: BTreeMap<&'a str, Vec<Link<'a>>>,
    internals: HashSet<&'a str>,
    merges: HashSet<&'a str>,
}

impl<'a> Catalog<'a> {
    pub(super) fn new(records: &'a Record) -> Self {
        let mut saved = Self {
            stores: BTreeMap::new(),
            internals: HashSet::new(),
            merges: HashSet::new(),
        };
        for (name, record) in records.iter().filter_map(|(n, v)| Some((n, v.section()?))) {
            for (key, spec) in record.iter().filter_map(|(k, v)| Some((k, v.section()?))) {
                if !key.starts_with("segment")
                    || spec.get("type").and_then(Value::text) != Some("snapshot")
                {
                    continue;
                }
                saved.internals.insert(name.as_str());
                for field in ["cow_store", "merge_store"] {
                    if let Some(store) = spec.get(field).and_then(Value::text) {
                        saved.stores.entry(store).or_default().push(Link {
                            internal: name,
                            spec,
                        });
                    }
                }
                if let Some(origin) = spec
                    .get("origin")
                    .and_then(Value::text)
                    .filter(|_| spec.contains_key("merge_store"))
                {
                    saved.merges.insert(origin);
                }
            }
        }
        saved
    }

    fn is_store(&self, name: &str) -> bool {
        self.stores.contains_key(name)
    }

    pub(super) fn is_unsafe_backing(&self, name: &str) -> bool {
        self.is_store(name) || self.internals.contains(name) || self.merges.contains(name)
    }

    pub(super) fn bind<R: Read + Seek>(
        &self,
        readers: &mut [R],
        candidates: &mut BTreeMap<&'a str, Candidate>,
        records: &Record,
        proof: &mut Sha256,
        checkpoint: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut budget = MAX_EXCEPTIONS;
        for name in &self.internals {
            candidates.get_mut(name).ok_or(BAD_FIELDS)?.visible = false;
        }
        let mut output = Vec::new();
        for (&store, links) in &self.stores {
            if !candidates.contains_key(store) || self.internals.contains(store) {
                continue;
            }
            checkpoint()?;
            // The visible name and UUID are those of the COW store.
            // Its size comes from the snapshot description, not the store.
            let size = candidates[links[0].internal].info.size;
            let bound = (|| {
                let [link] = links.as_slice() else {
                    return Err(BAD_RELATION.into());
                };
                self.backings(candidates, records, store, link)
            })();
            let result = match bound {
                Ok((origin, store_lv, chunk)) => {
                    proof.update(b"LVM2 snapshot store");
                    proof.update(store.as_bytes());
                    exception_table(
                        readers,
                        &store_lv,
                        size,
                        chunk,
                        proof,
                        &mut budget,
                        checkpoint,
                    )?
                    .map(|exceptions| Segment {
                        logical: 0..size,
                        unit: chunk,
                        mapping: Mapping::Snapshot(Image {
                            origin,
                            store: store_lv,
                            exceptions,
                        }),
                    })
                }
                Err(reason) => Err(reason),
            };
            output.push((store, size, result));
        }
        for (name, size, result) in output {
            let candidate = candidates.get_mut(name).ok_or(BAD_FIELDS)?;
            candidate.visible = true;
            candidate.info.size = size;
            candidate.bind("snapshot", result);
        }
        for name in &self.merges {
            if let Some(candidate) = candidates.get_mut(name) {
                let profile = candidate.info.profile.clone();
                candidate.bind(&profile, Err(MERGING.into()));
            }
        }
        Ok(())
    }

    fn backings(
        &self,
        candidates: &BTreeMap<&str, Candidate>,
        records: &Record,
        store: &str,
        link: &Link<'_>,
    ) -> Result<(Option<Arc<Volume>>, Arc<Volume>, u64), String> {
        let internal = field(records, link.internal, Value::section)?;
        let origin = field(link.spec, "origin", Value::text)?;
        if link.spec.contains_key("merge_store") || self.merges.contains(origin) {
            return Err(MERGING.into());
        }
        if !ordinary(internal)
            || field(internal, "segment_count", Value::number)? != 1
            || !link.spec.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "start_extent"
                        | "extent_count"
                        | "type"
                        | "origin"
                        | "cow_store"
                        | "chunk_size"
                        | "tags"
                )
            })
            || self.is_store(origin)
            || self.internals.contains(origin)
            || !ordinary(field(records, store, Value::section)?)
        {
            return Err(BAD_RELATION.into());
        }
        let origin_lv = candidates.get(origin).ok_or(BAD_RELATION)?;
        let origin_record = field(records, origin, Value::section)?;
        let chunk = bytes(field(link.spec, "chunk_size", Value::number)?)?;
        if !ordinary(origin_record)
            || origin_lv.info.size < candidates[link.internal].info.size
            || !(4096..=512 * 1024).contains(&chunk)
            || !chunk.is_power_of_two()
        {
            return Err(BAD_RELATION.into());
        }
        let store_lv = backing(&candidates[store])?;
        let origin = if !origin_lv.visible && is_zero(origin_record) {
            None
        } else {
            if !origin_lv.visible {
                return Err(BAD_RELATION.into());
            }
            Some(backing(origin_lv)?)
        };
        Ok((origin, store_lv, chunk))
    }
}

fn ordinary(record: &Record) -> bool {
    supported_metadata_flags(record)
        && record
            .get("status")
            .and_then(Value::list)
            .is_some_and(|flags| flags.iter().any(|flag| flag.text() == Some("READ")))
}

fn is_zero(record: &Record) -> bool {
    record.get("segment_count").and_then(Value::number) == Some(1)
        && record
            .get("segment1")
            .and_then(Value::section)
            .is_some_and(|spec| {
                spec.get("type").and_then(Value::text) == Some("zero")
                    && spec.keys().all(|key| {
                        matches!(
                            key.as_str(),
                            "start_extent" | "extent_count" | "type" | "tags"
                        )
                    })
            })
}

fn backing(candidate: &Candidate) -> Result<Arc<Volume>, String> {
    candidate.mapped.clone().ok_or_else(|| {
        if matches!(
            candidate.info.profile.as_str(),
            "linear" | "striped" | "mixed"
        ) {
            MISSING.into()
        } else {
            candidate
                .info
                .reason
                .clone()
                .unwrap_or_else(|| BAD_RELATION.into())
        }
    })
}

fn exception_table<R: Read + Seek>(
    readers: &mut [R],
    store: &Volume,
    size: u64,
    chunk: u64,
    proof: &mut Sha256,
    budget: &mut usize,
    checkpoint: &mut impl FnMut() -> Result<(), String>,
) -> Result<Result<Vec<(u64, u64)>, String>, String> {
    let store_chunks = store.size / chunk;
    if store_chunks < 3 {
        return Ok(Err(BAD_STORE.into()));
    }
    checkpoint()?;
    let mut area = vec![0; chunk as usize];
    let mut scratch = Vec::new();
    if store
        .read_exact_at(readers, 0, &mut area, &mut scratch)
        .is_err()
        || &area[..4] != b"SnAp"
        || u32le(&area, 4) != 1
        || u32le(&area, 8) != 1
        || u64::from(u32le(&area, 12)) != chunk / SECTOR
    {
        return Ok(Err(BAD_STORE.into()));
    }
    proof.update(&area);
    let origin_chunks = size.div_ceil(chunk);
    let stride = chunk / EXCEPTION_BYTES as u64 + 1;
    let mut entries = Vec::new();
    let mut locations = HashSet::new();
    for index in (1..store_chunks).step_by(stride as usize) {
        checkpoint()?;
        if store
            .read_exact_at(readers, index * chunk, &mut area, &mut scratch)
            .is_err()
        {
            return Ok(Err(BAD_STORE.into()));
        }
        proof.update(index.to_le_bytes());
        proof.update(&area);
        let mut terminal = false;
        for entry in area.chunks_exact(EXCEPTION_BYTES) {
            let old = u64le(entry, 0);
            let new = u64le(entry, 8);
            if new == 0 && old == 0 {
                terminal = true;
                continue;
            }
            if terminal
                || old >= origin_chunks
                || new == 0
                || new >= store_chunks
                || new % stride == 1
                || !locations.insert(new)
            {
                return Ok(Err(BAD_STORE.into()));
            }
            let Some(next) = budget.checked_sub(1) else {
                return Ok(Err(LIMIT.into()));
            };
            *budget = next;
            entries.push((old, new));
        }
        if terminal {
            entries.sort_unstable();
            if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return Ok(Err(BAD_STORE.into()));
            }
            return Ok(Ok(entries));
        }
    }
    Ok(Err(BAD_STORE.into()))
}
