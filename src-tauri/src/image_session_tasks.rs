use super::*;
use crate::{image_bridge, image_files};

pub fn select() -> Result<Option<image_files::Info>, String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    idle(&state())?;
    #[cfg(windows)]
    {
        image_files::choose()
    }
    #[cfg(not(windows))]
    {
        Err("当前仅支持 Windows 原生文件选择。".into())
    }
}
pub fn add(id: String) -> Result<image_files::Info, String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    idle(&state())?;
    #[cfg(windows)]
    {
        image_files::add(&id)
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("当前仅支持 Windows 原生文件选择。".into())
    }
}
pub fn discard(id: String) -> Result<(), String> {
    let _guard = crate::OPERATION
        .try_lock()
        .map_err(|_| "请等待当前磁盘任务完成")?;
    idle(&state())?;
    #[cfg(windows)]
    {
        image_files::discard(&id)
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("当前仅支持 Windows 原生文件选择。".into())
    }
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    Raw,
    Vhd,
    Vhdx,
    Vdi,
    Vmdk,
    Parallels,
}
impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Raw => "RAW",
            Self::Vhd => "VHD",
            Self::Vhdx => "VHDX",
            Self::Vdi => "VDI",
            Self::Vmdk => "VMDK",
            Self::Parallels => "Parallels",
        }
    }
    fn matches(self, name: &str) -> bool {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match self {
            Self::Raw => ["img", "raw"].contains(&ext.as_str()),
            Self::Vhd => ["vhd", "avhd"].contains(&ext.as_str()),
            Self::Vhdx => ext == "vhdx",
            Self::Vdi => ext == "vdi",
            Self::Vmdk => ext == "vmdk",
            Self::Parallels => ["hdd", "hds"].contains(&ext.as_str()),
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Descriptor {
    pub group: String,
    pub size: u64,
    pub format: Format,
    pub options: Options,
    pub independent: bool,
    pub notes: Vec<String>,
}
impl Descriptor {
    fn validate(&self, info: &image_files::Info) -> Result<(), String> {
        self.options.validate(self.size)?;
        let file = info.files.first().ok_or("缺少镜像主文件")?;
        if info.id != self.group || !self.format.matches(&file.name) {
            return Err("镜像类型与保留的主文件不匹配。".into());
        }
        if self.format == Format::Raw && (self.size != file.size || info.files.len() != 1) {
            return Err("RAW 镜像容量或附属文件选择不符。".into());
        }
        if !matches!(self.format, Format::Vhd | Format::Vmdk) && info.files.len() != 1 {
            return Err("此容器类型不接受父盘或分卷。".into());
        }
        if self.format != Format::Raw && self.options.kind != crate::image_browser::Kind::Disk {
            return Err("虚拟磁盘容器须按整盘镜像打开。".into());
        }
        if !matches!(self.format, Format::Raw | Format::Vhdx) && self.options.sector != 512 {
            return Err("此容器仅支持 512 B 逻辑扇区。".into());
        }
        if self.format == Format::Parallels && !self.independent {
            return Err("请确认所选 Parallels 文件是独立镜像，不依赖其他快照。".into());
        }
        if self.notes.len() > 128
            || self.notes.iter().map(String::len).sum::<usize>() > 65536
            || self
                .notes
                .iter()
                .any(|s| s.len() > 4096 || s.contains('\0'))
        {
            return Err("镜像说明超过长度限制。".into());
        }
        Ok(())
    }
}
pub fn attach(
    request: Descriptor,
    send: impl Fn(image_bridge::Event) -> Result<(), String> + Send + Sync + 'static,
) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = (request, send);
        Err("当前仅支持 Windows 原生镜像浏览。".into())
    }
    #[cfg(windows)]
    {
        let guard = crate::OPERATION
            .try_lock()
            .map_err(|_| "请等待当前磁盘任务完成")?;
        idle(&state())?;
        let group = image_files::take(&request.group)?;
        let info = group.info();
        request.validate(&info)?;
        group.validate()?;
        let bridge = image_bridge::Bridge::new(request.size, send)?;
        let (id, cancel) = start("打开镜像目录来源")?;
        state().pending = None;
        let work_id = id.clone();
        drop(guard);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = crate::OPERATION
                    .try_lock()
                    .map_err(|_| "另一项磁盘操作正在运行")?;
                let browser = Browser::from_factory(
                    Arc::new(image_bridge::Factory(bridge.clone())),
                    info.files[0].name.clone(),
                    request.options,
                    request.format.name().into(),
                    info.files.iter().map(|f| f.name.clone()).collect(),
                    request.notes.join("；"),
                    || progress(&work_id, &cancel, "检查镜像布局", 0, 0),
                )?;
                let opened = Opened {
                    browser: Arc::new(browser),
                    source_disks: group.disks(),
                    bridge: Some(bridge),
                    group,
                };
                opened.validate()?;
                commit(&mut state(), &work_id, opened)?;
                Ok((
                    "镜像已打开。选择分区并读取目录；没有挂载或修改镜像。".into(),
                    None,
                ))
            }))
            .unwrap_or_else(|_| Err("镜像读取异常退出，保留原镜像及目录。".into()));
            finish(&work_id, result);
        });
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair(format: Format, name: &str) -> (Descriptor, image_files::Info) {
        let info = image_files::Info {
            id: "group".into(),
            files: vec![image_files::FileInfo {
                id: "file".into(),
                name: name.into(),
                size: 4 * 1024 * 1024,
                last_modified: 0,
            }],
        };
        let request = Descriptor {
            group: info.id.clone(),
            size: 4 * 1024 * 1024,
            format,
            options: Options {
                kind: crate::image_browser::Kind::Disk,
                sector: 512,
            },
            independent: false,
            notes: vec![],
        };
        (request, info)
    }
    #[test]
    fn descriptor_binds_group_format_and_raw_extent_and_rejects_ambient_paths() {
        let (mut d, mut info) = pair(Format::Raw, "a.img");
        assert!(d.validate(&info).is_ok());
        d.size += 512;
        assert!(d.validate(&info).is_err());
        d.size -= 512;
        d.group = "stale".into();
        assert!(d.validate(&info).is_err());
        d.group = info.id.clone();
        info.files[0].name = "a.vhd".into();
        assert!(d.validate(&info).is_err());
        let json = serde_json::json!({"group":"id","size":4194304,"format":"raw","options":{"kind":"disk","sector":512},"independent":false,"notes":[],"path":"X:/arbitrary.img"});
        assert!(serde_json::from_value::<Descriptor>(json).is_err());
    }
    #[test]
    fn containers_require_whole_disk_geometry_and_explicit_parallels_independence() {
        for (format, name) in [
            (Format::Vhd, "a.avhd"),
            (Format::Vhdx, "a.vhdx"),
            (Format::Vdi, "a.vdi"),
            (Format::Vmdk, "a.vmdk"),
            (Format::Parallels, "a.hds"),
        ] {
            let (mut d, info) = pair(format, name);
            assert_eq!(d.validate(&info).is_ok(), format != Format::Parallels);
            d.independent = true;
            assert!(d.validate(&info).is_ok());
            d.options.sector = 4096;
            assert_eq!(d.validate(&info).is_ok(), format == Format::Vhdx);
            d.options.sector = 512;
            d.options.kind = crate::image_browser::Kind::Partition;
            assert!(d.validate(&info).is_err());
        }
    }
    #[test]
    fn attachments_and_notes_have_explicit_bounded_scopes() {
        for (format, name) in [
            (Format::Vhd, "a.vhd"),
            (Format::Vhdx, "a.vhdx"),
            (Format::Vdi, "a.vdi"),
            (Format::Vmdk, "a.vmdk"),
            (Format::Raw, "a.img"),
        ] {
            let (mut d, mut info) = pair(format, name);
            info.files.push(info.files[0].clone());
            assert_eq!(
                d.validate(&info).is_ok(),
                matches!(format, Format::Vhd | Format::Vmdk)
            );
            info.files.pop();
            d.notes = vec!["x".repeat(4097)];
            assert!(d.validate(&info).is_err());
            d.notes = vec!["x".repeat(4096); 17];
            assert!(d.validate(&info).is_err());
            d.notes = vec!["\0".into()];
            assert!(d.validate(&info).is_err());
        }
    }
}
