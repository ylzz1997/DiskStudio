//! Explicit reconstruction of Storage Spaces from retained read-only member images.
use super::Status;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRequest {
    id: String,
    space: String,
    sector: u64,
    allow_degraded: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    id: String,
    pool: String,
    members: Vec<String>,
    spaces: Vec<Space>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Space {
    id: String,
    name: String,
    size: Option<u64>,
    degraded: bool,
}

pub fn select() -> Result<Option<View>, String> {
    #[cfg(windows)]
    return platform::select();
    #[cfg(not(windows))]
    Err("存储空间成员镜像浏览当前仅支持 Windows。".into())
}

pub fn open(request: OpenRequest) -> Result<Status, String> {
    #[cfg(windows)]
    return platform::open(request);
    #[cfg(not(windows))]
    {
        let _ = request;
        Err("存储空间成员镜像浏览当前仅支持 Windows。".into())
    }
}

pub fn discard(id: String) {
    #[cfg(windows)]
    platform::discard(&id);
    #[cfg(not(windows))]
    let _ = id;
}

#[cfg(windows)]
mod platform {
    use super::super::{idle, replace, state, status_in, Opened};
    use super::*;
    use crate::{
        backup_archive::Source,
        data_io::Region,
        image_browser::{Browser, Kind, Options, ReaderFactory},
        image_files,
    };
    use disk_core::storage_spaces::{Group, Guid};
    use std::{
        fs::File,
        sync::{Arc, Mutex, MutexGuard, OnceLock},
    };

    const STALE: &str = "存储空间的成员或分配记录已变化，请重新选择成员镜像。";
    const TITLE: &str = "选择存储空间成员的 RAW/IMG 文件";
    type Pool = Group<Region<File>>;

    struct Identity {
        pool: Guid,
        database: [u8; 32],
        members: Vec<(Guid, [u8; 32])>,
    }

    struct Draft {
        files: Arc<image_files::Group>,
        identity: Identity,
        view: View,
    }

    fn draft() -> MutexGuard<'static, Option<Arc<Draft>>> {
        static DRAFT: OnceLock<Mutex<Option<Arc<Draft>>>> = OnceLock::new();
        DRAFT
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn read_pool(files: &image_files::Group) -> Result<Pool, String> {
        files.validate()?;
        let pool = Group::open(files.raw_readers()?, 4096)?;
        files.validate()?;
        Ok(pool)
    }

    impl Draft {
        fn validate(&self, pool: &Pool) -> Result<(), String> {
            if self.identity.pool != pool.metadata().pool
                || self.identity.database != pool.metadata().database_digest
                || self.identity.members != pool.member_layouts()
            {
                return Err(STALE.into());
            }
            Ok(())
        }
    }

    struct Factory {
        draft: Arc<Draft>,
        space: u64,
        allow_degraded: bool,
    }

    impl ReaderFactory for Factory {
        fn reader(&self) -> Result<Box<dyn Source>, String> {
            let pool = read_pool(&self.draft.files)?;
            self.draft.validate(&pool)?;
            Ok(Box::new(pool.into_reader(self.space, self.allow_degraded)?))
        }
    }

    pub(super) fn select() -> Result<Option<View>, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        let Some(files) = image_files::choose_raw_members(TITLE)? else {
            return Ok(None);
        };
        let mut pool = read_pool(&files)?;
        let info = files.info();
        let metadata = pool.metadata();
        let spaces = metadata
            .spaces
            .values()
            .filter(|space| !space.name.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        let identity = Identity {
            pool: metadata.pool,
            database: metadata.database_digest,
            members: pool.member_layouts(),
        };
        let name = metadata.name.clone();
        let view = View {
            id: info.id,
            pool: name,
            members: info.files.into_iter().map(|file| file.name).collect(),
            spaces: spaces
                .into_iter()
                .map(|space| {
                    let (size, degraded) = pool
                        .availability(space.id)
                        .map_or((None, false), |(size, degraded)| (Some(size), degraded));
                    Space {
                        id: uuid::Uuid::from_bytes(space.guid).to_string(),
                        name: space.name.clone(),
                        size,
                        degraded,
                    }
                })
                .collect(),
        };
        *draft() = Some(Arc::new(Draft {
            files,
            identity,
            view: view.clone(),
        }));
        Ok(Some(view))
    }

    pub(super) fn discard(id: &str) {
        let mut pending = draft();
        if pending.as_ref().is_some_and(|draft| draft.view.id == id) {
            *pending = None;
        }
    }

    pub(super) fn open(request: OpenRequest) -> Result<Status, String> {
        let _guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        if !matches!(request.sector, 512 | 4096) {
            return Err("请选择有效的存储空间逻辑扇区大小。".into());
        }
        let saved = draft()
            .as_ref()
            .filter(|saved| saved.view.id == request.id)
            .cloned()
            .ok_or(STALE)?;
        let chosen = saved
            .view
            .spaces
            .iter()
            .find(|space| space.id == request.space)
            .filter(|space| space.size.is_some())
            .ok_or("所选存储空间当前无法重组。")?;
        let mut pool = read_pool(&saved.files)?;
        saved.validate(&pool)?;
        let guid = uuid::Uuid::parse_str(&request.space)
            .map_err(|_| STALE)?
            .into_bytes();
        let space_id = pool
            .metadata()
            .spaces
            .values()
            .find(|space| space.guid == guid)
            .ok_or(STALE)?
            .id;
        let (size, degraded) = pool.availability(space_id)?;
        if Some(size) != chosen.size || degraded != chosen.degraded {
            return Err(STALE.into());
        }
        if degraded && !request.allow_degraded {
            return Err("此存储空间缺少镜像副本，请明确选择降级读取。".into());
        }
        let factory = Arc::new(Factory {
            draft: saved.clone(),
            space: space_id,
            allow_degraded: request.allow_degraded,
        });
        // The table checker requires an image filename. A space name is a display label.
        let mut browser = Browser::from_factory(
            factory,
            "storage-space.raw".into(),
            Options {
                kind: Kind::Disk,
                sector: request.sector,
            },
            "Storage Spaces".into(),
            saved.view.members.clone(),
            if degraded {
                crate::native_locale::text("此存储空间正在降级读取，部分镜像副本不可用。")
            } else {
                String::new()
            },
            || Ok(()),
        )?;
        browser.info.name.clone_from(&chosen.name);
        let opened = Opened {
            browser: Arc::new(browser),
            source_disks: saved.files.disks(),
            group: saved.files.clone(),
            bridge: None,
        };
        opened.validate()?;
        let mut pending = draft();
        if pending
            .as_ref()
            .is_none_or(|value| !Arc::ptr_eq(value, &saved))
        {
            return Err(STALE.into());
        }
        let mut state = state();
        idle(&state)?;
        replace(&mut state, opened);
        *pending = None;
        status_in(&state).ok_or(STALE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_binds_only_opaque_selections_and_explicit_degradation() {
        let request = serde_json::json!({"id":"selection","space":"volume","sector":4096,"allowDegraded":false});
        assert!(serde_json::from_value::<OpenRequest>(request.clone()).is_ok());
        let mut bad = request;
        bad["path"] = "Q:/member.raw".into();
        assert!(serde_json::from_value::<OpenRequest>(bad).is_err());
    }
}
