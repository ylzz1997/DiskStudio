//! LVM2 thin-pool bindings, including snapshots and external origins.
use super::*;
use thin_map::{Budget, Error, Pool, Run};

const BAD_COMPONENTS: &str = "LVM2 薄池组件缺失、重复或不受支持。";
const BAD_THIN_METADATA: &str = "LVM2 薄池元数据损坏、事务不一致或需要修复。";
const THIN_LIMIT: &str = "LVM2 薄池元数据超出本次读取限制。";
const MISSING: &str = "LVM2 薄置备卷需要未选择的物理卷。";
const POOL_ONLY: &str = "请选择此薄池中的逻辑卷。";

pub(super) struct Image {
    data: Arc<Volume>,
    metadata_degraded: bool,
    external_origin: Option<Arc<Volume>>,
    runs: Vec<Run>,
}

impl Image {
    pub(super) fn degraded_raid(&self) -> bool {
        self.metadata_degraded
            || self.data.degraded_raid
            || self
                .external_origin
                .as_ref()
                .is_some_and(|origin| origin.degraded_raid)
    }

    pub(super) fn location(&self, position: u64, chunk: u64) -> (Option<&Volume>, u64, u64) {
        let block = position / chunk;
        let index = self.runs.partition_point(|run| run.logical.end <= block);
        let before = if let Some(run) = self.runs.get(index) {
            if run.logical.contains(&block) {
                let offset = position - run.logical.start * chunk;
                return (
                    Some(&self.data),
                    run.physical * chunk + offset,
                    run.logical.end * chunk - position,
                );
            }
            run.logical.start * chunk - position
        } else {
            u64::MAX
        };
        if let Some(origin) = self
            .external_origin
            .as_deref()
            .filter(|lv| position < lv.size)
        {
            return (Some(origin), position, before.min(origin.size - position));
        }
        (None, 0, before)
    }
}

struct ThinVolume<'a> {
    pool: &'a str,
    id: u64,
    transaction: u64,
    origin: Option<&'a str>,
    external: Option<&'a str>,
}

impl<'a> ThinVolume<'a> {
    fn parse(segment: &'a Record) -> Result<Self, String> {
        let id = field(segment, "device_id", Value::number)?;
        let pool = field(segment, "thin_pool", Value::text)?;
        let transaction = field(segment, "transaction_id", Value::number)?;
        if id >= (1 << 24)
            || !segment.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "start_extent"
                        | "extent_count"
                        | "type"
                        | "thin_pool"
                        | "device_id"
                        | "transaction_id"
                        | "origin"
                        | "external_origin"
                        | "tags"
                )
            })
        {
            return Err(BAD_COMPONENTS.into());
        }
        let name = |key| {
            segment
                .get(key)
                .map(|v| v.text().ok_or(BAD_COMPONENTS))
                .transpose()
        };
        Ok(Self {
            pool,
            id,
            transaction,
            origin: name("origin")?,
            external: name("external_origin")?,
        })
    }
}

struct Backing {
    chunk: u64,
    metadata: Arc<Volume>,
    data: Arc<Volume>,
    pool: Pool,
}

fn segment(record: &Record) -> Option<&Record> {
    (record.get("segment_count")?.number()? == 1).then_some(record.get("segment1")?.section()?)
}

fn segment_type(record: &Record) -> Option<&str> {
    segment(record)?.get("type")?.text()
}

fn component(
    candidates: &BTreeMap<&str, Candidate>,
    name: &str,
    hidden: bool,
) -> Result<Arc<Volume>, String> {
    let candidate = candidates
        .get(name)
        .filter(|entry| !hidden || !entry.visible)
        .ok_or(BAD_COMPONENTS)?;
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
                .unwrap_or_else(|| BAD_COMPONENTS.into())
        }
    })
}

struct PoolConfig<'a> {
    chunk: u64,
    transaction: u64,
    metadata: &'a str,
    data: &'a str,
}

impl<'a> PoolConfig<'a> {
    fn parse(record: &'a Record) -> Result<Self, String> {
        let spec = segment(record)
            .filter(|_| segment_type(record) == Some("thin-pool"))
            .ok_or(BAD_COMPONENTS)?;
        if !supported(record)
            || !spec.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "start_extent"
                        | "extent_count"
                        | "type"
                        | "metadata"
                        | "pool"
                        | "transaction_id"
                        | "chunk_size"
                        | "discards"
                        | "zero_new_blocks"
                        | "crop_metadata"
                        | "tags"
                )
            })
        {
            return Err(BAD_COMPONENTS.into());
        }
        let chunk = bytes(field(spec, "chunk_size", Value::number)?)?;
        let metadata = field(spec, "metadata", Value::text)?;
        let data = field(spec, "pool", Value::text)?;
        let transaction = field(spec, "transaction_id", Value::number)?;
        if !(65536..=1024 * super::super::MIB).contains(&chunk)
            || chunk % 65536 != 0
            || metadata == data
        {
            return Err(BAD_COMPONENTS.into());
        }
        Ok(Self {
            chunk,
            transaction,
            metadata,
            data,
        })
    }
}

fn has_status(record: &Record, status: &str) -> bool {
    record
        .get("status")
        .and_then(Value::list)
        .is_some_and(|items| items.iter().any(|item| item.text() == Some(status)))
}

fn supported(record: &Record) -> bool {
    has_status(record, "READ") && supported_metadata_flags(record)
}

fn pool_result<T>(result: Result<T, Error>) -> Result<Result<T, String>, String> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(Error::Invalid) => Ok(Err(BAD_THIN_METADATA.into())),
        Err(Error::Limit) => Ok(Err(THIN_LIMIT.into())),
        Err(Error::Stopped(error)) => Err(error),
    }
}

pub(super) fn bind<'a, R: Read + Seek>(
    readers: &mut [R],
    candidates: &mut BTreeMap<&'a str, Candidate>,
    records: &'a Record,
    snapshots: &snapshot::Catalog<'_>,
    proof: &mut Sha256,
    checkpoint: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let configs: BTreeMap<_, _> = records
        .iter()
        .filter_map(|(name, value)| {
            let record = value.section()?;
            (segment_type(record) == Some("thin-pool"))
                .then(|| (name.as_str(), PoolConfig::parse(record)))
        })
        .collect();
    let mut thin = BTreeMap::new();
    let mut ids = BTreeMap::new();
    for (name, value) in records {
        let record = value.section().ok_or(BAD_FIELDS)?;
        if let Some(spec) = segment(record).filter(|_| segment_type(record) == Some("thin")) {
            if let (Some(pool), Some(id)) = (
                spec.get("thin_pool").and_then(Value::text),
                spec.get("device_id").and_then(Value::number),
            ) {
                *ids.entry((pool, id)).or_insert(0usize) += 1;
            }
            thin.insert(name.as_str(), ThinVolume::parse(spec));
        }
    }
    let mut owners = BTreeMap::new();
    for config in configs.values().filter_map(|entry| entry.as_ref().ok()) {
        for name in [config.data, config.metadata] {
            *owners.entry(name).or_insert(0usize) += 1;
        }
    }
    let mut results = Vec::new();
    let mut pools = BTreeMap::new();
    let mut budget = Budget::default();
    for (&name, entry) in &thin {
        let candidate = candidates.get(name).ok_or(BAD_FIELDS)?;
        if !candidate.visible {
            continue;
        }
        checkpoint()?;
        let record = records
            .get(name)
            .and_then(Value::section)
            .ok_or(BAD_FIELDS)?;
        let profile = if segment(record).is_some_and(|seg| seg.contains_key("origin")) {
            "thinSnapshot"
        } else {
            "thin"
        };
        let result = (|| {
            let thin_lv = entry.as_ref().map_err(Clone::clone)?;
            if !supported(record)
                || ids.get(&(thin_lv.pool, thin_lv.id)) != Some(&1)
                || thin_lv.origin.is_some_and(|origin| {
                    origin == name
                        || !thin.get(origin).is_some_and(|entry| {
                            entry
                                .as_ref()
                                .is_ok_and(|origin| origin.pool == thin_lv.pool)
                        })
                })
            {
                return Err(BAD_COMPONENTS.into());
            }
            let external = thin_lv
                .external
                .map(|origin| {
                    if owners.contains_key(origin)
                        || snapshots.is_unsafe_backing(origin)
                        || !records
                            .get(origin)
                            .and_then(Value::section)
                            .is_some_and(|record| supported(record) && !has_status(record, "WRITE"))
                    {
                        return Err(BAD_COMPONENTS.into());
                    }
                    component(candidates, origin, false)
                })
                .transpose()?;
            let config = configs
                .get(thin_lv.pool)
                .ok_or(BAD_COMPONENTS)?
                .as_ref()
                .map_err(Clone::clone)?;
            if thin_lv.transaction > config.transaction
                || owners.get(config.metadata) != Some(&1)
                || owners.get(config.data) != Some(&1)
                || snapshots.is_unsafe_backing(config.metadata)
                || snapshots.is_unsafe_backing(config.data)
            {
                return Err(BAD_COMPONENTS.into());
            }
            Ok((thin_lv, external, config))
        })();
        let (thin_lv, external_origin, config) = match result {
            Ok(bound) => bound,
            Err(reason) => {
                results.push((name, profile, Err(reason)));
                continue;
            }
        };
        let bound = match pools.entry(thin_lv.pool) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let backing = match (
                    component(candidates, config.metadata, true),
                    component(candidates, config.data, true),
                ) {
                    (Ok(metadata), Ok(data)) if candidates[thin_lv.pool].info.size == data.size => {
                        proof.update(b"LVM2 thin metadata");
                        proof.update(thin_lv.pool.as_bytes());
                        pool_result(Pool::open(
                            readers,
                            &metadata,
                            data.size,
                            config.chunk,
                            config.transaction,
                            &mut budget,
                            proof,
                            checkpoint,
                        ))?
                        .map(|pool| Backing {
                            chunk: config.chunk,
                            metadata,
                            data,
                            pool,
                        })
                    }
                    (Err(reason), _) | (_, Err(reason)) => Err(reason),
                    _ => Err(BAD_COMPONENTS.into()),
                };
                entry.insert(backing)
            }
        };
        let result = match bound {
            Ok(pool) => {
                proof.update(name.as_bytes());
                let size = candidate.info.size;
                pool_result(pool.pool.mapping(
                    readers,
                    &pool.metadata,
                    (thin_lv.id, thin_lv.transaction),
                    size.div_ceil(pool.chunk),
                    &mut budget,
                    proof,
                    checkpoint,
                ))?
                .map(|runs| Segment {
                    logical: 0..size,
                    unit: pool.chunk,
                    mapping: Mapping::Thin(Image {
                        data: pool.data.clone(),
                        metadata_degraded: pool.metadata.degraded_raid,
                        external_origin,
                        runs,
                    }),
                })
            }
            Err(reason) => Err(reason.clone()),
        };
        results.push((name, profile, result));
    }
    for name in configs.keys() {
        let candidate = candidates.get_mut(name).ok_or(BAD_FIELDS)?;
        candidate.bind("thinPool", Err(POOL_ONLY.into()));
    }
    for (name, profile, result) in results {
        candidates
            .get_mut(name)
            .ok_or(BAD_FIELDS)?
            .bind(profile, result);
    }
    Ok(())
}
