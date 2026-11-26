//! Native offline workspace for the Windows PE session of the same executable.
#[path = "winpe_export.rs"]
mod export;
#[path = "winpe_files.rs"]
mod files;
#[path = "winpe_image.rs"]
mod image;
#[path = "winpe_partitions.rs"]
mod partitions;
#[path = "winpe_physical.rs"]
mod physical;
#[path = "winpe_password.rs"]
mod password;
#[path = "winpe_registration.rs"]
mod registration;

use crate::{
    data_tasks::native_file_picker,
    image_browser::{Kind, Options},
    image_table_conversion::{registration as layout_options, Scheme},
    native_locale::{self, Language, Locale},
    recovery::ScanMode,
    recovery_tree::ROOT,
};
use std::{
    cell::RefCell,
    collections::BTreeSet,
    io,
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Globalization::GetUserDefaultUILanguage,
        Graphics::Gdi::{GetStockObject, COLOR_WINDOW, DEFAULT_GUI_FONT, SYSTEM_FIXED_FONT},
        System::{
            LibraryLoader::GetModuleHandleW,
            Registry::{RegCloseKey, RegOpenKeyExW, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE},
        },
        UI::{
            Controls::EM_SETLIMITTEXT,
            Input::KeyboardAndMouse::{EnableWindow, SetFocus},
            WindowsAndMessaging::*,
        },
    },
};

const CLASS: windows_sys::core::PCWSTR = w!("DiskStudioWinPE");
const OPEN: i32 = 101;
const GEOMETRY: i32 = 102;
const REPORT: i32 = 103;
const PARTITION: i32 = 104;
const MODE: i32 = 105;
const SCAN: i32 = 106;
const BOOT: i32 = 107;
const LIST: i32 = 108;
const ACTIVATE: i32 = 109;
const CANCEL: i32 = 110;
const EXPORT: i32 = 111;
const TABLE: i32 = 112;
const LBA: i32 = 113;
const SECTOR: i32 = 114;
const FIND: i32 = 115;
const SCOPE: i32 = 116;
const RANGE_FIRST: i32 = 117;
const RANGE_LAST: i32 = 118;
const FIRST_LABEL: i32 = 119;
const LAST_LABEL: i32 = 120;
const MARK: i32 = 121;
const NEW_TABLE: i32 = 122;
const MBR_FORM: i32 = 123;
const REGISTER: i32 = 124;
const REFRESH_DISKS: i32 = 125;
const DISKS: i32 = 126;
const OPEN_DISK: i32 = 127;
const RECOVER_DISK: i32 = 128;
const TICK: usize = 1;

enum Completed {
    NoHandoff,
    Image(image::Opened),
    Disks(Vec<physical::Choice>),
    Files(files::Listing),
    Preview(String),
    Sector(String),
    Search(partitions::Results),
    RegistrationPreview(Box<registration::Plan>),
    RegistrationBound(Box<registration::Prepared>),
    RecoveryPreview(Box<physical::Recovery>),
    Prepared(Box<export::Prepared>),
    Exported(String),
}

enum WorkProgress {
    Handoff,
    Search(Arc<partitions::Progress>),
    Copy(Arc<registration::Progress>),
}

struct Pending {
    result: Receiver<Result<Completed, String>>,
    cancel: Arc<AtomicBool>,
    focus: i32,
    writing: bool,
    progress: Option<WorkProgress>,
    progress_text: String,
}

#[derive(Default)]
struct Workspace {
    disks: Vec<physical::Choice>,
    source: Option<Arc<image::Source>>,
    boot: String,
    volumes: Vec<Arc<partitions::Volume>>,
    marked: BTreeSet<usize>,
    registration: Option<Box<registration::Plan>>,
    recovery: Option<Box<physical::Recovery>>,
    listing: Option<Arc<files::Listing>>,
    folder: String,
    offset: usize,
    visible: Vec<files::Row>,
}

thread_local! {
    static WORK: RefCell<Option<Pending>> = const { RefCell::new(None) };
    static SPACE: RefCell<Workspace> = RefCell::new(Workspace::default());
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

pub(crate) fn detected() -> bool {
    let mut key = null_mut();
    // SAFETY: the fixed registry key is queried with read access only.
    unsafe {
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SYSTEM\\CurrentControlSet\\Control\\MiniNT"),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        RegCloseKey(key);
    }
    true
}

pub(crate) fn run() {
    // SAFETY: the locale query has no inputs and the entire window lifecycle
    // runs on this thread.
    unsafe {
        let language = match GetUserDefaultUILanguage() {
            0x0404 | 0x0c04 | 0x1404 => Language::ZhTw,
            lang if lang & 0x03ff == 4 => Language::ZhCn,
            _ => Language::En,
        };
        native_locale::set(language);
        if let Err(error) = window() {
            let message = wide(&error.to_string());
            MessageBoxW(
                null_mut(),
                message.as_ptr(),
                w!("DiskStudio · WinPE"),
                MB_ICONERROR,
            );
        }
    }
}

unsafe fn window() -> io::Result<()> {
    let instance = GetModuleHandleW(null());
    let class = WNDCLASSW {
        lpfnWndProc: Some(on_message),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_WINDOW + 1) as _,
        lpszClassName: CLASS,
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(io::Error::last_os_error());
    }
    let hwnd = CreateWindowExW(
        WS_EX_CONTROLPARENT,
        CLASS,
        w!("DiskStudio · WinPE"),
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        860,
        640,
        null_mut(),
        null_mut(),
        instance,
        null(),
    );
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    ShowWindow(hwnd, SW_SHOW);
    SetFocus(GetDlgItem(hwnd, OPEN));
    begin(
        hwnd,
        OPEN,
        native_locale::current().text("正在检查 WinPE 续接任务…"),
        true,
        |cancel| match crate::firmware_boot::claim_winpe_task()? {
            Some(task) => task.run(cancel).map(Completed::Exported),
            None => Ok(Completed::NoHandoff),
        },
    );
    WORK.with(|work| {
        if let Some(pending) = work.borrow_mut().as_mut() {
            pending.progress = Some(WorkProgress::Handoff);
        }
    });
    let mut message = MSG::default();
    loop {
        match GetMessageW(&mut message, null_mut(), 0, 0) {
            -1 => return Err(io::Error::last_os_error()),
            0 => return Ok(()),
            _ if IsDialogMessageW(hwnd, &message) != 0 => {}
            _ => {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

unsafe fn control(
    parent: HWND,
    id: i32,
    class: windows_sys::core::PCWSTR,
    label: &str,
    style: u32,
    extended_style: u32,
) -> HWND {
    let label = wide(label);
    let tab_stop = if matches!(id, FIRST_LABEL | LAST_LABEL) {
        0
    } else {
        WS_TABSTOP
    };
    let hwnd = CreateWindowExW(
        extended_style,
        class,
        label.as_ptr(),
        WS_CHILD | WS_VISIBLE | tab_stop | style,
        0,
        0,
        0,
        0,
        parent,
        id as usize as _,
        GetModuleHandleW(null()),
        null(),
    );
    if !hwnd.is_null() {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            GetStockObject(DEFAULT_GUI_FONT) as usize,
            0,
        );
    }
    hwnd
}

unsafe fn controls(hwnd: HWND) -> bool {
    let locale = native_locale::current();
    let geometry = control(
        hwnd,
        GEOMETRY,
        w!("ComboBox"),
        "",
        CBS_DROPDOWNLIST as u32,
        0,
    );
    let open = control(
        hwnd,
        OPEN,
        w!("Button"),
        locale.text("打开镜像并检查…"),
        0,
        0,
    );
    let refresh = control(
        hwnd,
        REFRESH_DISKS,
        w!("Button"),
        locale.text("刷新磁盘"),
        0,
        0,
    );
    let disks = control(hwnd, DISKS, w!("ComboBox"), "", CBS_DROPDOWNLIST as u32, 0);
    let open_disk = control(
        hwnd,
        OPEN_DISK,
        w!("Button"),
        locale.text("只读打开磁盘"),
        0,
        0,
    );
    let report = control(
        hwnd,
        REPORT,
        w!("Edit"),
        locale.text("打开 RAW 或 IMG 镜像，或刷新磁盘后选择实体磁盘。"),
        (ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32 | WS_VSCROLL,
        WS_EX_CLIENTEDGE,
    );
    let partition = control(
        hwnd,
        PARTITION,
        w!("ComboBox"),
        "",
        CBS_DROPDOWNLIST as u32,
        0,
    );
    let mode = control(hwnd, MODE, w!("ComboBox"), "", CBS_DROPDOWNLIST as u32, 0);
    let scan = control(hwnd, SCAN, w!("Button"), locale.text("读取文件目录"), 0, 0);
    let boot = control(hwnd, BOOT, w!("Button"), locale.text("启动检查结果"), 0, 0);
    let cancel = control(hwnd, CANCEL, w!("Button"), locale.text("取消读取"), 0, 0);
    let table = control(hwnd, TABLE, w!("Button"), locale.text("检查分区表"), 0, 0);
    let lba = control(
        hwnd,
        LBA,
        w!("Edit"),
        "0",
        ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE,
    );
    let sector = control(
        hwnd,
        SECTOR,
        w!("Button"),
        locale.text("查看 LBA 扇区"),
        0,
        0,
    );
    let scope = control(hwnd, SCOPE, w!("ComboBox"), "", CBS_DROPDOWNLIST as u32, 0);
    let first_label = control(
        hwnd,
        FIRST_LABEL,
        w!("Static"),
        locale.text("起始 LBA"),
        0,
        0,
    );
    let last_label = control(
        hwnd,
        LAST_LABEL,
        w!("Static"),
        locale.text("结束 LBA"),
        0,
        0,
    );
    let first = control(
        hwnd,
        RANGE_FIRST,
        w!("Edit"),
        "0",
        ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE,
    );
    let last = control(
        hwnd,
        RANGE_LAST,
        w!("Edit"),
        "",
        ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE,
    );
    let find = control(hwnd, FIND, w!("Button"), locale.text("搜索丢失分区"), 0, 0);
    let mark = control(hwnd, MARK, w!("Button"), locale.text("加入登记"), 0, 0);
    let new_table = control(
        hwnd,
        NEW_TABLE,
        w!("ComboBox"),
        "",
        CBS_DROPDOWNLIST as u32,
        0,
    );
    let mbr_form = control(
        hwnd,
        MBR_FORM,
        w!("ComboBox"),
        "",
        CBS_DROPDOWNLIST as u32,
        0,
    );
    let register = control(hwnd, REGISTER, w!("Button"), "", 0, 0);
    let recover = control(hwnd, RECOVER_DISK, w!("Button"), "", 0, 0);
    let list = control(
        hwnd,
        LIST,
        w!("ListBox"),
        "",
        (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32 | WS_VSCROLL,
        WS_EX_CLIENTEDGE,
    );
    let entry = control(
        hwnd,
        ACTIVATE,
        w!("Button"),
        locale.text("打开所选项"),
        0,
        0,
    );
    let export = control(hwnd, EXPORT, w!("Button"), locale.text("导出所选项…"), 0, 0);
    if [
        geometry,
        open,
        refresh,
        disks,
        open_disk,
        report,
        partition,
        mode,
        scan,
        boot,
        cancel,
        list,
        entry,
        export,
        table,
        lba,
        sector,
        scope,
        first_label,
        last_label,
        first,
        last,
        find,
        mark,
        new_table,
        mbr_form,
        register,
        recover,
    ]
    .iter()
    .any(|handle| handle.is_null())
    {
        return false;
    }
    for field in [lba, first, last] {
        SendMessageW(field, EM_SETLIMITTEXT, 22, 0);
    }
    for choice in [
        "整盘镜像 · 512 B",
        "整盘镜像 · 4096 B",
        "分区镜像 · 512 B",
        "分区镜像 · 4096 B",
    ] {
        let text = wide(locale.text(choice));
        if SendMessageW(geometry, CB_ADDSTRING, 0, text.as_ptr() as LPARAM) < 0 {
            return false;
        }
    }
    SendMessageW(geometry, CB_SETCURSEL, 0, 0);
    SendMessageW(disks, CB_SETDROPPEDWIDTH, 330, 0);
    if !populate_disks(hwnd, &[], &locale) {
        return false;
    }
    for choice in ["现存文件", "删除候选"] {
        let text = wide(locale.text(choice));
        if SendMessageW(mode, CB_ADDSTRING, 0, text.as_ptr() as LPARAM) < 0 {
            return false;
        }
    }
    SendMessageW(mode, CB_SETCURSEL, 0, 0);
    for choice in ["整盘镜像", "全部未分配空间", "指定 LBA 范围"] {
        let text = wide(locale.text(choice));
        if SendMessageW(scope, CB_ADDSTRING, 0, text.as_ptr() as LPARAM) < 0 {
            return false;
        }
    }
    SendMessageW(scope, CB_SETCURSEL, 0, 0);
    for (combo, choices) in [
        (new_table, &["空白镜像用 GPT", "空白镜像用 MBR"][..]),
        (
            mbr_form,
            &["MBR 自动放置", "MBR 主分区", "MBR 逻辑分区"][..],
        ),
    ] {
        for choice in choices {
            let text = wide(locale.text(choice));
            if SendMessageW(combo, CB_ADDSTRING, 0, text.as_ptr() as LPARAM) < 0 {
                return false;
            }
        }
        SendMessageW(combo, CB_SETCURSEL, 0, 0);
    }
    set_buttons(hwnd, false);
    true
}

unsafe fn layout(hwnd: HWND) {
    let mut area = RECT::default();
    if GetClientRect(hwnd, &mut area) == 0 {
        return;
    }
    let combo_width = (area.right / 3).clamp(145, 250);
    MoveWindow(GetDlgItem(hwnd, GEOMETRY), 16, 14, combo_width, 152, 1);
    MoveWindow(GetDlgItem(hwnd, OPEN), 24 + combo_width, 14, 153, 26, 1);
    MoveWindow(GetDlgItem(hwnd, BOOT), 185 + combo_width, 14, 130, 26, 1);
    MoveWindow(
        GetDlgItem(hwnd, REFRESH_DISKS),
        323 + combo_width,
        14,
        104,
        26,
        1,
    );
    MoveWindow(
        GetDlgItem(hwnd, OPEN_DISK),
        435 + combo_width,
        14,
        130,
        26,
        1,
    );
    MoveWindow(GetDlgItem(hwnd, PARTITION), 16, 49, combo_width, 196, 1);
    MoveWindow(GetDlgItem(hwnd, MODE), 24 + combo_width, 49, 102, 130, 1);
    MoveWindow(GetDlgItem(hwnd, SCAN), 134 + combo_width, 49, 106, 26, 1);
    MoveWindow(GetDlgItem(hwnd, CANCEL), 248 + combo_width, 49, 94, 26, 1);
    MoveWindow(
        GetDlgItem(hwnd, DISKS),
        350 + combo_width,
        49,
        (area.right - 366 - combo_width).max(1),
        196,
        1,
    );
    MoveWindow(GetDlgItem(hwnd, TABLE), 16, 84, 144, 26, 1);
    MoveWindow(GetDlgItem(hwnd, LBA), 168, 84, 136, 26, 1);
    MoveWindow(GetDlgItem(hwnd, SECTOR), 312, 84, 144, 26, 1);
    MoveWindow(GetDlgItem(hwnd, SCOPE), 16, 136, 152, 116, 1);
    MoveWindow(GetDlgItem(hwnd, FIRST_LABEL), 176, 119, 98, 17, 1);
    MoveWindow(GetDlgItem(hwnd, LAST_LABEL), 282, 119, 98, 17, 1);
    MoveWindow(GetDlgItem(hwnd, RANGE_FIRST), 176, 136, 98, 26, 1);
    MoveWindow(GetDlgItem(hwnd, RANGE_LAST), 282, 136, 98, 26, 1);
    MoveWindow(GetDlgItem(hwnd, FIND), 388, 136, 152, 26, 1);
    MoveWindow(GetDlgItem(hwnd, MARK), 16, 171, 112, 26, 1);
    MoveWindow(GetDlgItem(hwnd, NEW_TABLE), 136, 171, 132, 112, 1);
    MoveWindow(GetDlgItem(hwnd, MBR_FORM), 276, 171, 120, 132, 1);
    MoveWindow(GetDlgItem(hwnd, REGISTER), 404, 171, 148, 26, 1);
    MoveWindow(GetDlgItem(hwnd, RECOVER_DISK), 560, 171, 193, 26, 1);
    const CONTENT_TOP: i32 = 211;
    let has_listing = SPACE.with(|space| space.borrow().listing.is_some());
    let list_width = (area.right / 3).clamp(175, 320);
    let report_left = if has_listing { list_width + 24 } else { 16 };
    ShowWindow(
        GetDlgItem(hwnd, LIST),
        if has_listing { SW_SHOW } else { SW_HIDE },
    );
    ShowWindow(
        GetDlgItem(hwnd, ACTIVATE),
        if has_listing { SW_SHOW } else { SW_HIDE },
    );
    ShowWindow(
        GetDlgItem(hwnd, EXPORT),
        if has_listing { SW_SHOW } else { SW_HIDE },
    );
    MoveWindow(
        GetDlgItem(hwnd, LIST),
        16,
        CONTENT_TOP,
        list_width,
        (area.bottom - CONTENT_TOP - 51).max(1),
        1,
    );
    let left_button = (list_width - 8) / 2;
    MoveWindow(
        GetDlgItem(hwnd, ACTIVATE),
        16,
        (area.bottom - 42).max(CONTENT_TOP),
        left_button,
        26,
        1,
    );
    MoveWindow(
        GetDlgItem(hwnd, EXPORT),
        24 + left_button,
        (area.bottom - 42).max(CONTENT_TOP),
        list_width - left_button - 8,
        26,
        1,
    );
    MoveWindow(
        GetDlgItem(hwnd, REPORT),
        report_left,
        CONTENT_TOP,
        (area.right - report_left - 16).max(1),
        (area.bottom - CONTENT_TOP - 15).max(1),
        1,
    );
}

unsafe fn set_buttons(hwnd: HWND, busy: bool) {
    let selection = selected_volume_index(hwnd);
    let (opened, disk_image, blank_table, gpt, partitions, listing, review, count, marked) = SPACE
        .with(|space| {
            let state = space.borrow();
            let info = state.source.as_ref().map(|source| &source.browser.info);
            (
                info.is_some(),
                info.is_some_and(|info| info.options.kind != Kind::Partition),
                info.is_some_and(|info| info.options.kind == Kind::Search),
                info.is_some_and(|info| info.partition_table.as_deref() == Some("GPT")),
                !state.volumes.is_empty(),
                state.listing.is_some(),
                state.registration.is_some(),
                state.marked.len(),
                selection.is_some_and(|index| state.marked.contains(&index)),
            )
        });
    let (can_recover, reviewing_disk) = SPACE.with(|space| {
        let state = space.borrow();
        (
            state.source.as_deref().is_some_and(physical::can_recover),
            state.recovery.is_some(),
        )
    });
    let reviewing = review || reviewing_disk;
    let available = !busy && !reviewing;
    let disks = SPACE.with(|space| !space.borrow().disks.is_empty());
    let selected_disk = SendMessageW(GetDlgItem(hwnd, DISKS), CB_GETCURSEL, 0, 0) > 0;
    let locale = native_locale::current();
    let mark_label = locale.text(if marked {
        "移出登记"
    } else {
        "加入登记"
    });
    let register_label = if review {
        locale.text("保存新镜像…").to_owned()
    } else {
        locale.format("预览登记 ({count})", &[("count", &count.to_string())])
    };
    let recover_label = if reviewing_disk {
        locale.text("确认写入实体盘…").to_owned()
    } else {
        locale.format(
            "预览登记到实体盘 ({count})",
            &[("count", &count.to_string())],
        )
    };
    let cancel_label = locale.text(if reviewing {
        "取消预览"
    } else {
        "取消读取"
    });
    SetDlgItemTextW(hwnd, MARK, wide(mark_label).as_ptr());
    SetDlgItemTextW(hwnd, REGISTER, wide(&register_label).as_ptr());
    SetDlgItemTextW(hwnd, RECOVER_DISK, wide(&recover_label).as_ptr());
    SetDlgItemTextW(hwnd, CANCEL, wide(cancel_label).as_ptr());
    let exportable =
        available && selected_row(hwnd).is_some_and(|(listing, row)| listing.exportable(&row));
    let volume = selected_volume(hwnd);
    let browseable = available && volume.as_ref().is_some_and(|volume| volume.can_browse());
    let registerable =
        available && volume.is_some_and(|volume| volume.registration_candidate().is_some());
    let custom =
        disk_image && available && SendMessageW(GetDlgItem(hwnd, SCOPE), CB_GETCURSEL, 0, 0) == 2;
    for (id, enabled) in [
        (OPEN, available),
        (REFRESH_DISKS, available),
        (DISKS, available && disks),
        (OPEN_DISK, available && selected_disk && disks),
        (GEOMETRY, available),
        (BOOT, opened && available),
        (TABLE, disk_image && available),
        (LBA, opened && available),
        (SECTOR, opened && available),
        (SCOPE, disk_image && available),
        (FIND, disk_image && available),
        (RANGE_FIRST, custom),
        (RANGE_LAST, custom),
        (FIRST_LABEL, custom),
        (LAST_LABEL, custom),
        (PARTITION, partitions && available),
        (MODE, browseable),
        (SCAN, browseable),
        (LIST, listing && available),
        (ACTIVATE, listing && available),
        (EXPORT, exportable),
        (MARK, registerable),
        (NEW_TABLE, blank_table && available && count != 0),
        (MBR_FORM, disk_image && !gpt && available && count != 0),
        (REGISTER, review && !busy || available && count != 0),
        (
            RECOVER_DISK,
            reviewing_disk && !busy || available && can_recover && count != 0,
        ),
        (CANCEL, busy || reviewing),
    ] {
        EnableWindow(GetDlgItem(hwnd, id), i32::from(enabled));
    }
}

unsafe fn set_report(hwnd: HWND, report: &str) {
    let control = GetDlgItem(hwnd, REPORT);
    SendMessageW(
        control,
        WM_SETFONT,
        GetStockObject(DEFAULT_GUI_FONT) as usize,
        1,
    );
    let message = wide(&report.replace('\n', "\r\n"));
    SetWindowTextW(control, message.as_ptr());
    SendMessageW(control, WM_VSCROLL, SB_TOP as usize, 0);
}

enum PathMode {
    Image,
    Directory,
    RegisteredImage,
}

unsafe fn choose_path(hwnd: HWND, mode: PathMode) -> Result<Option<PathBuf>, String> {
    let locale = native_locale::current();
    let (title, filter, suggested, extension) = match mode {
        PathMode::Image => (
            locale.text("选择 RAW 或 IMG 镜像"),
            "RAW / IMG\0*.raw;*.img\0".to_owned(),
            None,
            None,
        ),
        PathMode::Directory => (
            locale.text("选择新建导出目录的位置和名称"),
            format!("{}\0*\0", locale.text("导出目录")),
            Some(format!("DiskStudio-recovered-{}", uuid::Uuid::new_v4())),
            None,
        ),
        PathMode::RegisteredImage => (
            locale.text("保存含找回分区的新 RAW 镜像"),
            "RAW\0*.img\0".to_owned(),
            Some("DiskStudio-recovered-partitions.img".into()),
            Some("img"),
        ),
    };
    native_file_picker::choose(
        hwnd, title, &filter, suggested.as_deref(), extension, matches!(mode, PathMode::Image),
    )
}

unsafe fn begin(
    hwnd: HWND,
    focus: i32,
    status: &str,
    writing: bool,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<Completed, String> + Send + 'static,
) {
    if WORK.with(|work| work.borrow().is_some()) {
        return;
    }
    let (send, result) = mpsc::channel();
    if SetTimer(hwnd, TICK, 120, None) == 0 {
        set_report(hwnd, &io::Error::last_os_error().to_string());
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let active = Arc::clone(&cancel);
    if let Err(error) = std::thread::Builder::new().spawn(move || {
        let _ = send.send(work(active));
    }) {
        KillTimer(hwnd, TICK);
        set_report(hwnd, &error.to_string());
        return;
    }
    WORK.with(|work| {
        *work.borrow_mut() = Some(Pending {
            result,
            cancel,
            focus,
            writing,
            progress: None,
            progress_text: String::new(),
        })
    });
    set_buttons(hwnd, true);
    set_report(hwnd, status);
}

unsafe fn open_image(hwnd: HWND) {
    let index = SendMessageW(GetDlgItem(hwnd, GEOMETRY), CB_GETCURSEL, 0, 0);
    let (kind, sector) = match index {
        0 => (Kind::Disk, 512),
        1 => (Kind::Disk, 4096),
        2 => (Kind::Partition, 512),
        3 => (Kind::Partition, 4096),
        _ => return,
    };
    let path = match choose_path(hwnd, PathMode::Image) {
        Ok(Some(path)) => path,
        Ok(None) => return,
        Err(error) => {
            set_report(hwnd, &error);
            return;
        }
    };
    let locale = native_locale::current();
    let worker_locale = locale.clone();
    begin(
        hwnd,
        OPEN,
        locale.text("正在读取镜像并检查启动文件…"),
        false,
        move |cancel| {
            image::open(&path, Options { kind, sector }, &worker_locale, &cancel)
                .map(Completed::Image)
        },
    );
}

unsafe fn populate_disks(hwnd: HWND, disks: &[physical::Choice], locale: &Locale) -> bool {
    let combo = GetDlgItem(hwnd, DISKS);
    SendMessageW(combo, CB_RESETCONTENT, 0, 0);
    for label in std::iter::once(locale.text("选择实体磁盘"))
        .chain(disks.iter().map(|disk| disk.label.as_str()))
    {
        if SendMessageW(combo, CB_ADDSTRING, 0, wide(label).as_ptr() as LPARAM) < 0 {
            SendMessageW(combo, CB_RESETCONTENT, 0, 0);
            return false;
        }
    }
    SendMessageW(combo, CB_SETCURSEL, 0, 0);
    true
}

unsafe fn refresh_disks(hwnd: HWND) {
    let locale = native_locale::current();
    populate_disks(hwnd, &[], &locale);
    SPACE.with(|space| space.borrow_mut().disks.clear());
    set_buttons(hwnd, false);
    let worker_locale = locale.clone();
    begin(
        hwnd,
        REFRESH_DISKS,
        locale.text("正在读取实体磁盘清单…"),
        false,
        move |cancel| physical::inventory(&worker_locale, &cancel).map(Completed::Disks),
    );
}

unsafe fn open_disk(hwnd: HWND) {
    let selected = SendMessageW(GetDlgItem(hwnd, DISKS), CB_GETCURSEL, 0, 0);
    let choice = usize::try_from(selected).ok().and_then(|row| {
        let position = row.checked_sub(1)?;
        SPACE.with(|space| space.borrow().disks.get(position).cloned())
    });
    let Some(choice) = choice else { return };
    let locale = native_locale::current();
    let worker_locale = locale.clone();
    begin(
        hwnd,
        OPEN_DISK,
        locale.text("正在只读检查实体磁盘…"),
        false,
        move |cancel| physical::open(choice, &worker_locale, &cancel).map(Completed::Image),
    );
}

unsafe fn check_table(hwnd: HWND) {
    let Some(source) = SPACE.with(|space| space.borrow().source.clone()) else {
        return;
    };
    let locale = native_locale::current();
    let worker_locale = locale.clone();
    begin(
        hwnd,
        TABLE,
        locale.text("正在检查镜像分区表…"),
        false,
        move |cancel| {
            source
                .table(&worker_locale, &cancel)
                .map(Completed::Preview)
        },
    );
}

fn parse_lba(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let result = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) if hex.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            u64::from_str_radix(hex, 16)
        }
        _ if text.bytes().all(|byte| byte.is_ascii_digit()) => text.parse(),
        _ => return Err("请输入十进制或以 0x 开头的 LBA。".into()),
    };
    result.map_err(|_| "请输入十进制或以 0x 开头的 LBA。".into())
}

#[test]
fn parses_only_unsigned_decimal_or_prefixed_hex_lba() {
    assert_eq!(parse_lba(" 2048 ").unwrap(), 2048);
    assert_eq!(parse_lba("0X800").unwrap(), 2048);
    for invalid in [
        "",
        "+1",
        "-1",
        "0x",
        "0xGG",
        "1 000",
        "18446744073709551616",
    ] {
        assert!(parse_lba(invalid).is_err(), "{invalid}");
    }
}

unsafe fn view_sector(hwnd: HWND) {
    let Some(source) = SPACE.with(|space| space.borrow().source.clone()) else {
        return;
    };
    let locale = native_locale::current();
    let lba = match edit_lba(hwnd, LBA) {
        Ok(lba) => lba,
        Err(error) => {
            set_report(hwnd, locale.text(&error));
            SetFocus(GetDlgItem(hwnd, LBA));
            return;
        }
    };
    begin(
        hwnd,
        LBA,
        locale.text("正在读取镜像扇区…"),
        false,
        move |cancel| source.sector(lba, &cancel).map(Completed::Sector),
    );
}

unsafe fn edit_lba(hwnd: HWND, field: i32) -> Result<u64, String> {
    let mut buffer = [0u16; 23];
    let length = GetDlgItemTextW(hwnd, field, buffer.as_mut_ptr(), buffer.len() as i32);
    parse_lba(&String::from_utf16_lossy(&buffer[..length as usize]))
}

unsafe fn find_partitions(hwnd: HWND) {
    let Some(source) = SPACE.with(|space| space.borrow().source.clone()) else {
        return;
    };
    let locale = native_locale::current();
    let scope = match SendMessageW(GetDlgItem(hwnd, SCOPE), CB_GETCURSEL, 0, 0) {
        0 => partitions::Scope::Whole,
        1 => partitions::Scope::Unallocated,
        2 => {
            let input = [RANGE_FIRST, RANGE_LAST].map(|field| edit_lba(hwnd, field));
            match input {
                [Ok(first), Ok(last)] => partitions::Scope::Lba { first, last },
                [Err(error), _] => {
                    set_report(hwnd, locale.text(&error));
                    SetFocus(GetDlgItem(hwnd, RANGE_FIRST));
                    return;
                }
                [Ok(_), Err(error)] => {
                    set_report(hwnd, locale.text(&error));
                    SetFocus(GetDlgItem(hwnd, RANGE_LAST));
                    return;
                }
            }
        }
        _ => return,
    };
    let progress = Arc::new(partitions::Progress::default());
    let worker_progress = progress.clone();
    begin(
        hwnd,
        FIND,
        locale.text("正在核对分区搜索范围…"),
        false,
        move |cancel| {
            partitions::search(source, scope, cancel, &worker_progress).map(Completed::Search)
        },
    );
    WORK.with(|work| {
        if let Some(pending) = work
            .borrow_mut()
            .as_mut()
            .filter(|pending| pending.focus == FIND)
        {
            pending.progress = Some(WorkProgress::Search(progress));
        }
    });
}

unsafe fn show_boot_report(hwnd: HWND) {
    let report = SPACE.with(|space| space.borrow().boot.clone());
    show_source_report(hwnd, &report);
}

unsafe fn show_source_report(hwnd: HWND, report: &str) {
    SPACE.with(|space| {
        let mut state = space.borrow_mut();
        state.listing = None;
        state.visible.clear();
    });
    set_report(hwnd, report);
    layout(hwnd);
    set_buttons(hwnd, false);
}

unsafe fn selected_volume(hwnd: HWND) -> Option<Arc<partitions::Volume>> {
    let index = selected_volume_index(hwnd)?;
    SPACE.with(|space| space.borrow().volumes.get(index).cloned())
}

unsafe fn selected_volume_index(hwnd: HWND) -> Option<usize> {
    let control = GetDlgItem(hwnd, PARTITION);
    let row = usize::try_from(SendMessageW(control, CB_GETCURSEL, 0, 0)).ok()?;
    usize::try_from(SendMessageW(control, CB_GETITEMDATA, row, 0)).ok()
}

unsafe fn show_volume_report(hwnd: HWND) {
    let locale = native_locale::current();
    if let Some(report) = selected_volume(hwnd).and_then(|volume| volume.describe(&locale)) {
        show_source_report(hwnd, &report);
    } else {
        show_boot_report(hwnd);
    }
}

unsafe fn populate_volumes(hwnd: HWND, source: Arc<image::Source>, found: Vec<partitions::Volume>) {
    let parts = &source.browser.info.partitions;
    let preferred = if found.is_empty() { 0 } else { parts.len() };
    SPACE.with(|space| {
        let mut state = space.borrow_mut();
        state.volumes = parts
            .iter()
            .map(|part| partitions::Volume::table(source.clone(), part.id))
            .chain(found)
            .map(Arc::new)
            .collect();
        state.marked.clear();
    });
    draw_volume_choices(hwnd, preferred);
}

unsafe fn draw_volume_choices(hwnd: HWND, preferred: usize) {
    let locale = native_locale::current();
    let control = GetDlgItem(hwnd, PARTITION);
    SendMessageW(control, CB_RESETCONTENT, 0, 0);
    let titles = SPACE.with(|space| {
        let state = space.borrow();
        state
            .volumes
            .iter()
            .enumerate()
            .filter_map(|(index, volume)| {
                let mut title = volume.title(&locale).ok()?;
                if state.marked.contains(&index) {
                    title.insert_str(0, "+ ");
                }
                Some((index, wide(&title)))
            })
            .collect::<Vec<_>>()
    });
    let mut selected_row = 0;
    for (index, title) in titles {
        let row = SendMessageW(control, CB_ADDSTRING, 0, title.as_ptr() as LPARAM);
        if row < 0 {
            break;
        }
        if SendMessageW(control, CB_SETITEMDATA, row as usize, index as LPARAM) < 0 {
            SendMessageW(control, CB_DELETESTRING, row as usize, 0);
            break;
        }
        if index == preferred {
            selected_row = row as usize;
        }
    }
    SendMessageW(control, CB_SETCURSEL, selected_row, 0);
}

unsafe fn toggle_registration_mark(hwnd: HWND) {
    let Some(index) = selected_volume_index(hwnd) else {
        return;
    };
    let changed = SPACE.with(|space| {
        let mut state = space.borrow_mut();
        if state
            .volumes
            .get(index)
            .and_then(|volume| volume.registration_candidate())
            .is_none()
        {
            return false;
        }
        if state.marked.remove(&index) {
            return true;
        }
        if state.marked.len() >= 128 {
            return false;
        }
        state.marked.insert(index)
    });
    if changed {
        draw_volume_choices(hwnd, index);
        set_buttons(hwnd, false);
    } else {
        set_report(
            hwnd,
            native_locale::current().text("请选择 1–128 个候选，仅恢复分区记录。"),
        );
    }
}

unsafe fn register_partitions(hwnd: HWND) {
    let locale = native_locale::current();
    let worker_locale = locale.clone();
    if SPACE.with(|space| space.borrow().registration.is_some()) {
        let path = match choose_path(hwnd, PathMode::RegisteredImage) {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                set_report(hwnd, &error);
                return;
            }
        };
        let Some(plan) = SPACE.with(|space| space.borrow_mut().registration.take()) else {
            return;
        };
        begin(
            hwnd,
            REGISTER,
            locale.text("正在核对登记来源和目标…"),
            false,
            move |cancel| {
                plan.bind(&path, &worker_locale, &cancel)
                    .map(|plan| Completed::RegistrationBound(Box::new(plan)))
            },
        );
        return;
    }
    let Some((source, selected)) = marked_volumes() else {
        return;
    };
    let new_table = if SendMessageW(GetDlgItem(hwnd, NEW_TABLE), CB_GETCURSEL, 0, 0) == 1 {
        Scheme::Mbr
    } else {
        Scheme::Gpt
    };
    let form = if source.browser.info.partition_table.as_deref() == Some("GPT") {
        layout_options::Form::Auto
    } else {
        match SendMessageW(GetDlgItem(hwnd, MBR_FORM), CB_GETCURSEL, 0, 0) {
            1 => layout_options::Form::Primary,
            2 => layout_options::Form::Logical,
            _ => layout_options::Form::Auto,
        }
    };
    let options = layout_options::Options { new_table, form };
    begin(
        hwnd,
        REGISTER,
        locale.text("核对候选分区边界"),
        false,
        move |cancel| {
            registration::Plan::new(source, &selected, options, &cancel)
                .map(|plan| Completed::RegistrationPreview(Box::new(plan)))
        },
    );
}

fn marked_volumes() -> Option<(Arc<image::Source>, Vec<Arc<partitions::Volume>>)> {
    SPACE.with(|space| {
        let state = space.borrow();
        let source = state.source.as_ref()?.clone();
        let selected = state
            .marked
            .iter()
            .map(|&index| state.volumes.get(index).cloned())
            .collect::<Option<Vec<_>>>()?;
        Some((source, selected))
    })
}

unsafe fn recover_disk(hwnd: HWND) {
    if WORK.with(|work| work.borrow().is_some()) {
        return;
    }
    let locale = native_locale::current();
    if let Some(plan) = SPACE.with(|space| space.borrow_mut().recovery.take()) {
        let details = wide(&plan.summary(&locale));
        let title = wide(locale.text("确认将候选登记到实体盘"));
        if MessageBoxW(
            hwnd,
            details.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING,
        ) == IDYES
        {
            let worker_locale = locale.clone();
            begin(
                hwnd,
                RECOVER_DISK,
                locale.text("正在核对并写入实体盘 GPT…"),
                true,
                move |cancel| {
                    plan.execute(&worker_locale, &cancel)
                        .map(Completed::Exported)
                },
            );
        } else {
            set_report(hwnd, locale.text("已取消登记。"));
            set_buttons(hwnd, false);
        }
        return;
    }
    let Some((source, selected)) = marked_volumes() else {
        return;
    };
    begin(
        hwnd,
        RECOVER_DISK,
        locale.text("核对候选分区边界"),
        false,
        move |cancel| {
            physical::Recovery::prepare(&source, &selected, &cancel)
                .map(|plan| Completed::RecoveryPreview(Box::new(plan)))
        },
    );
}

unsafe fn read_directory(hwnd: HWND) {
    let Some(volume) = selected_volume(hwnd).filter(|volume| volume.can_browse()) else {
        return;
    };
    let mode = if SendMessageW(GetDlgItem(hwnd, MODE), CB_GETCURSEL, 0, 0) == 1 {
        ScanMode::Deleted
    } else {
        ScanMode::Existing
    };
    let locale = native_locale::current();
    begin(
        hwnd,
        LIST,
        locale.text("正在读取镜像文件目录…"),
        false,
        move |cancel| files::scan(volume, mode, cancel).map(Completed::Files),
    );
}

unsafe fn show_folder(hwnd: HWND) {
    let locale = native_locale::current();
    let Some((summary, rows)) = SPACE.with(|space| {
        let space = space.borrow();
        space
            .listing
            .as_ref()
            .map(|listing| listing.page(&space.folder, space.offset, &locale))
    }) else {
        return;
    };
    let list = GetDlgItem(hwnd, LIST);
    SendMessageW(list, LB_RESETCONTENT, 0, 0);
    let mut visible = Vec::with_capacity(rows.len());
    for (label, row) in rows {
        let text = wide(&label);
        if SendMessageW(list, LB_ADDSTRING, 0, text.as_ptr() as LPARAM) < 0 {
            break;
        }
        visible.push(row);
    }
    if !visible.is_empty() {
        SendMessageW(list, LB_SETCURSEL, 0, 0);
    }
    SPACE.with(|space| space.borrow_mut().visible = visible);
    set_report(hwnd, &summary);
    layout(hwnd);
    set_buttons(hwnd, false);
}

unsafe fn selected_row(hwnd: HWND) -> Option<(Arc<files::Listing>, files::Row)> {
    let index = SendMessageW(GetDlgItem(hwnd, LIST), LB_GETCURSEL, 0, 0);
    SPACE.with(|space| {
        let state = space.borrow();
        Some((
            state.listing.as_ref()?.clone(),
            state.visible.get(index as usize)?.clone(),
        ))
    })
}

unsafe fn request_export(hwnd: HWND) {
    let Some((listing, row)) = selected_row(hwnd) else {
        return;
    };
    if !listing.exportable(&row) {
        return;
    }
    let path = match choose_path(hwnd, PathMode::Directory) {
        Ok(Some(path)) => path,
        Ok(None) => return,
        Err(error) => {
            set_report(hwnd, &error);
            return;
        }
    };
    let locale = native_locale::current();
    let worker_locale = locale.clone();
    begin(
        hwnd,
        EXPORT,
        locale.text("正在核对导出来源和目标…"),
        false,
        move |cancel| {
            export::prepare(&listing, &row, &path, &worker_locale, &cancel)
                .map(|result| Completed::Prepared(Box::new(result)))
        },
    );
}

unsafe fn activate(hwnd: HWND) {
    let Some((listing, row)) = selected_row(hwnd) else {
        return;
    };
    match row {
        files::Row::Folder(folder) | files::Row::Up(folder) => {
            SPACE.with(|space| {
                let mut state = space.borrow_mut();
                state.folder = folder;
                state.offset = 0;
            });
            show_folder(hwnd);
        }
        files::Row::Offset(offset) => {
            SPACE.with(|space| space.borrow_mut().offset = offset);
            show_folder(hwnd);
        }
        files::Row::File(index) => {
            let locale = native_locale::current();
            let worker_locale = locale.clone();
            begin(
                hwnd,
                LIST,
                locale.text("正在读取文件预览…"),
                false,
                move |cancel| {
                    listing
                        .preview(index, &worker_locale, cancel)
                        .map(Completed::Preview)
                },
            );
        }
    }
}

unsafe fn finish(hwnd: HWND) {
    if password::poll(hwnd) {
        return;
    }
    let result = WORK.with(|work| {
        let mut work = work.borrow_mut();
        let pending = work.as_ref()?;
        let result = pending.result.try_recv();
        if matches!(result, Err(TryRecvError::Empty)) {
            return None;
        }
        let pending = work.take()?;
        Some((
            result,
            pending.focus,
            pending.cancel.load(Ordering::Relaxed),
            pending.writing,
        ))
    });
    if let Some((result, focus, cancelled, writing)) = result {
        KillTimer(hwnd, TICK);
        let locale = native_locale::current();
        let disk_write = writing && focus == RECOVER_DISK;
        let focus = if disk_write {
            SPACE.with(|space| *space.borrow_mut() = Workspace::default());
            populate_disks(hwnd, &[], &locale);
            SendMessageW(GetDlgItem(hwnd, PARTITION), CB_RESETCONTENT, 0, 0);
            layout(hwnd);
            REFRESH_DISKS
        } else {
            focus
        };
        let focus = match result {
            Ok(Ok(Completed::NoHandoff)) => {
                set_report(hwnd, locale.text("打开 RAW 或 IMG 镜像，或刷新磁盘后选择实体磁盘。"));
                OPEN
            }
            Ok(Ok(Completed::Search(results))) => {
                let mut report = results.describe(&locale);
                if let Some(source) = SPACE.with(|space| space.borrow().source.clone()) {
                    populate_volumes(hwnd, source, results.volumes);
                    if let Some(details) =
                        selected_volume(hwnd).and_then(|volume| volume.describe(&locale))
                    {
                        report.push_str("\n\n");
                        report.push_str(&details);
                    }
                }
                show_source_report(hwnd, &report);
                focus
            }
            _ if cancelled && !writing => {
                set_report(hwnd, locale.text("读取已取消。"));
                focus
            }
            Ok(Ok(Completed::Disks(disks))) => {
                if !populate_disks(hwnd, &disks, &locale) {
                    set_report(hwnd, locale.text("无法显示实体磁盘清单。"));
                    REFRESH_DISKS
                } else if disks.is_empty() {
                    set_report(hwnd, locale.text("没有可核对身份的实体磁盘。"));
                    REFRESH_DISKS
                } else {
                    SPACE.with(|space| space.borrow_mut().disks = disks);
                    set_report(hwnd, locale.text("请选择要只读打开的实体磁盘。"));
                    DISKS
                }
            }
            Ok(Ok(Completed::Image(opened))) => {
                SetDlgItemTextW(hwnd, LBA, w!("0"));
                SetDlgItemTextW(hwnd, RANGE_FIRST, w!("0"));
                let info = &opened.source.browser.info;
                let end = wide(&(info.size / info.options.sector - 1).to_string());
                SetDlgItemTextW(hwnd, RANGE_LAST, end.as_ptr());
                let scope = GetDlgItem(hwnd, SCOPE);
                let whole = wide(locale.text(if opened.source.disk().is_some() {
                    "整块磁盘"
                } else {
                    "整盘镜像"
                }));
                if SendMessageW(scope, CB_INSERTSTRING, 0, whole.as_ptr() as LPARAM) == 0 {
                    SendMessageW(scope, CB_DELETESTRING, 1, 0);
                }
                SendMessageW(scope, CB_SETCURSEL, 0, 0);
                SendMessageW(GetDlgItem(hwnd, NEW_TABLE), CB_SETCURSEL, 0, 0);
                SendMessageW(GetDlgItem(hwnd, MBR_FORM), CB_SETCURSEL, 0, 0);
                let source = Arc::new(opened.source);
                SPACE.with(|space| {
                    let mut state = space.borrow_mut();
                    *state = Workspace {
                        disks: std::mem::take(&mut state.disks),
                        source: Some(source.clone()),
                        boot: opened.report,
                        ..Workspace::default()
                    }
                });
                populate_volumes(hwnd, source, Vec::new());
                show_boot_report(hwnd);
                focus
            }
            Ok(Ok(Completed::Files(listing))) => {
                SPACE.with(|space| {
                    let mut state = space.borrow_mut();
                    state.listing = Some(Arc::new(listing));
                    state.folder = ROOT.into();
                    state.offset = 0;
                });
                show_folder(hwnd);
                focus
            }
            Ok(Ok(Completed::RegistrationPreview(plan))) => {
                let report = plan.describe(&locale);
                SPACE.with(|space| space.borrow_mut().registration = Some(plan));
                show_source_report(hwnd, &report);
                REGISTER
            }
            Ok(Ok(Completed::RecoveryPreview(plan))) => {
                let report = plan.preview(&locale);
                SPACE.with(|space| space.borrow_mut().recovery = Some(plan));
                show_source_report(hwnd, &report);
                RECOVER_DISK
            }
            Ok(Ok(Completed::RegistrationBound(plan))) => {
                let details = wide(&plan.describe(&locale));
                let title = wide(locale.text("确认登记到新镜像"));
                if MessageBoxW(
                    hwnd,
                    details.as_ptr(),
                    title.as_ptr(),
                    MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING,
                ) == IDYES
                {
                    let worker_locale = locale.clone();
                    let progress = Arc::new(registration::Progress::default());
                    let worker_progress = progress.clone();
                    begin(
                        hwnd,
                        REGISTER,
                        locale.text("正在核对登记来源和目标…"),
                        true,
                        move |cancel| {
                            plan.execute(&worker_locale, &cancel, &worker_progress)
                                .map(Completed::Exported)
                        },
                    );
                    WORK.with(|work| {
                        if let Some(pending) = work.borrow_mut().as_mut() {
                            pending.progress = Some(WorkProgress::Copy(progress));
                        }
                    });
                    return;
                }
                set_report(hwnd, locale.text("已取消登记。"));
                REGISTER
            }
            Ok(Ok(Completed::Prepared(plan))) => {
                set_buttons(hwnd, false);
                let details = wide(&plan.describe(&locale));
                let title = wide(locale.text("确认导出所选内容"));
                if MessageBoxW(
                    hwnd,
                    details.as_ptr(),
                    title.as_ptr(),
                    MB_YESNO | MB_DEFBUTTON2 | MB_ICONWARNING,
                ) == IDYES
                {
                    let worker_locale = locale.clone();
                    begin(
                        hwnd,
                        EXPORT,
                        locale.text("正在导出并回读校验文件…"),
                        true,
                        move |cancel| {
                            plan.execute(&worker_locale, cancel)
                                .map(Completed::Exported)
                        },
                    );
                    return;
                }
                set_report(hwnd, locale.text("已取消导出。"));
                focus
            }
            Ok(Ok(Completed::Preview(report) | Completed::Exported(report))) => {
                set_report(hwnd, &report);
                focus
            }
            Ok(Ok(Completed::Sector(report))) => {
                set_report(hwnd, &report);
                SendMessageW(
                    GetDlgItem(hwnd, REPORT),
                    WM_SETFONT,
                    GetStockObject(SYSTEM_FIXED_FONT) as usize,
                    1,
                );
                focus
            }
            Ok(Err(error)) => {
                set_report(hwnd, locale.text(&error));
                if writing
                    || matches!(
                        focus,
                        EXPORT
                            | TABLE
                            | LBA
                            | FIND
                            | REGISTER
                            | RECOVER_DISK
                            | REFRESH_DISKS
                            | OPEN_DISK
                    )
                {
                    focus
                } else {
                    OPEN
                }
            }
            Err(_) => {
                set_report(
                    hwnd,
                    locale.text(if disk_write {
                        "实体盘登记未能完成。请刷新磁盘并核对实际状态。"
                    } else if writing {
                        "导出未能完成。请检查目标目录中已保留的结果。"
                    } else {
                        "只读检查未能完成。"
                    }),
                );
                if disk_write {
                    REFRESH_DISKS
                } else {
                    OPEN
                }
            }
        };
        set_buttons(hwnd, false);
        SetFocus(GetDlgItem(hwnd, focus));
    } else {
        let locale = native_locale::current();
        let message = WORK.with(|work| {
            let mut work = work.borrow_mut();
            let pending = work.as_mut()?;
            let progress = pending.progress.as_ref()?;
            let canceled = pending.cancel.load(Ordering::Relaxed);
            let message = match (progress, canceled) {
                (WorkProgress::Handoff, true) => locale.text("正在取消 WinPE 续接任务…").to_owned(),
                (WorkProgress::Handoff, false) => {
                    let job = crate::data_tasks::status()?;
                    let phase = locale.text(&job.phase);
                    let percent = (u128::from(job.done) * 100).checked_div(u128::from(job.total));
                    match percent {
                        Some(percent) => format!("{phase} · {}%", percent.min(100)),
                        None => phase.to_owned(),
                    }
                }
                (WorkProgress::Search(_), true) => locale.text("正在停止分区搜索…").to_owned(),
                (WorkProgress::Copy(_), true) => locale
                    .text("正在停止保存镜像。未完成文件将保留。")
                    .to_owned(),
                (WorkProgress::Search(progress), false) => progress.message(&locale),
                (WorkProgress::Copy(progress), false) => progress.message(&locale),
            };
            if pending.progress_text == message {
                return None;
            }
            pending.progress_text = message.clone();
            Some(message)
        });
        if let Some(message) = message {
            set_report(hwnd, &message);
        }
    }
}

unsafe fn cancel_work(hwnd: HWND, closing: bool) -> bool {
    let pending = WORK.with(|work| {
        let work = work.borrow();
        let pending = work.as_ref()?;
        if !closing || pending.writing {
            pending.cancel.store(true, Ordering::Relaxed);
        }
        Some((pending.writing, pending.focus))
    });
    if !closing && pending.is_none() {
        let preview = SPACE.with(|space| {
            let mut state = space.borrow_mut();
            if state.recovery.take().is_some() {
                Some(RECOVER_DISK)
            } else {
                state.registration.take().map(|_| REGISTER)
            }
        });
        if let Some(id) = preview {
            show_volume_report(hwnd);
            SetFocus(GetDlgItem(hwnd, id));
        }
    }
    let Some((writing, focus)) = pending else {
        return false;
    };
    if writing {
        set_report(
            hwnd,
            native_locale::current().text(if focus == RECOVER_DISK {
                "正在停止实体盘登记。请核对磁盘实际状态。"
            } else if focus == REGISTER {
                "正在停止保存镜像。未完成文件将保留。"
            } else {
                "正在停止导出。已写入的结果和清单将保留。"
            }),
        );
    }
    writing
}

unsafe extern "system" fn on_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            if !controls(hwnd) {
                return -1;
            }
            layout(hwnd);
        }
        WM_GETMINMAXINFO => {
            let limits = &mut *(lparam as *mut MINMAXINFO);
            limits.ptMinTrackSize.x = 840;
            limits.ptMinTrackSize.y = 375;
        }
        WM_SIZE => layout(hwnd),
        WM_COMMAND => {
            let control = (wparam & 0xffff) as i32;
            let notification = (wparam >> 16) as u32;
            match (control, notification) {
                (OPEN, BN_CLICKED) => open_image(hwnd),
                (REFRESH_DISKS, BN_CLICKED) => refresh_disks(hwnd),
                (OPEN_DISK, BN_CLICKED) => open_disk(hwnd),
                (TABLE, BN_CLICKED) => check_table(hwnd),
                (SECTOR, BN_CLICKED) => view_sector(hwnd),
                (FIND, BN_CLICKED) => find_partitions(hwnd),
                (MARK, BN_CLICKED) => toggle_registration_mark(hwnd),
                (REGISTER, BN_CLICKED) => register_partitions(hwnd),
                (RECOVER_DISK, BN_CLICKED) => recover_disk(hwnd),
                (SCAN, BN_CLICKED) => read_directory(hwnd),
                (EXPORT, BN_CLICKED) => request_export(hwnd),
                (ACTIVATE, BN_CLICKED) | (LIST, LBN_DBLCLK) => activate(hwnd),
                (LIST, LBN_SELCHANGE) | (SCOPE | DISKS, CBN_SELCHANGE) => {
                    set_buttons(hwnd, WORK.with(|work| work.borrow().is_some()))
                }
                (BOOT, BN_CLICKED) => show_boot_report(hwnd),
                (PARTITION | MODE, CBN_SELCHANGE) => show_volume_report(hwnd),
                (CANCEL, BN_CLICKED) => {
                    cancel_work(hwnd, false);
                }
                _ => return DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
        WM_CLOSE if cancel_work(hwnd, true) => {}
        WM_TIMER if wparam == TICK => finish(hwnd),
        WM_DESTROY => PostQuitMessage(0),
        _ => return DefWindowProcW(hwnd, message, wparam, lparam),
    }
    0
}
