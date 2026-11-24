//! Enter an archive password in WinPE without saving it in the handoff record.
use crate::{backup_encryption::Password, backup_secrets, native_locale};
use std::{cell::Cell, ptr::null};
use windows_sys::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Controls::EM_SETLIMITTEXT, WindowsAndMessaging::*},
    },
};
use zeroize::Zeroizing;

const PASSWORD: i32 = 101;
thread_local! { static ACTIVE: Cell<bool> = const { Cell::new(false) }; }

pub(super) unsafe fn poll(hwnd: HWND) -> bool {
    if ACTIVE.with(Cell::get) {
        return true;
    }
    let Some(prompt) = backup_secrets::status() else {
        return false;
    };
    ACTIVE.with(|active| active.set(true));
    let value = if prompt.purpose == backup_secrets::Purpose::Unlock {
        ask(hwnd, &prompt.filename)
    } else {
        None
    };
    ACTIVE.with(|active| active.set(false));
    let answer = value.map_or(backup_secrets::Answer::Cancel, |value| {
        backup_secrets::Answer::Password {
            value,
            confirmation: None,
        }
    });
    let _ = backup_secrets::submit(backup_secrets::Reply {
        id: prompt.id,
        answer,
    });
    true
}

fn string(out: &mut Vec<u16>, value: &str) {
    out.extend(value.encode_utf16().chain([0]));
}

fn dword(out: &mut Vec<u16>, value: u32) {
    out.extend([value as u16, (value >> 16) as u16]);
}

fn control(out: &mut Vec<u16>, style: u32, rect: [u16; 4], id: i32, class: u16, label: &str) {
    if out.len() % 2 != 0 {
        out.push(0);
    }
    dword(out, WS_CHILD | WS_VISIBLE | style);
    dword(out, 0);
    out.extend(rect);
    out.extend([id as u16, 0xffff, class]);
    string(out, label);
    out.push(0);
}

unsafe fn ask(hwnd: HWND, filename: &str) -> Option<Password> {
    let locale = native_locale::current();
    let name: String = filename
        .chars()
        .filter(|c| !c.is_control())
        .take(88)
        .collect();
    let mut words = Vec::with_capacity(240);
    dword(
        &mut words,
        WS_POPUP
            | WS_CAPTION
            | WS_SYSMENU
            | DS_MODALFRAME as u32
            | DS_SETFONT as u32
            | DS_CENTER as u32,
    );
    dword(&mut words, 0);
    words.extend([4, 0, 0, 250, 78, 0, 0]);
    string(&mut words, locale.text("解锁备份"));
    words.push(9);
    string(&mut words, "Segoe UI");
    control(&mut words, 0, [9, 8, 232, 18], -1, 0x82, &name);
    control(
        &mut words,
        WS_TABSTOP | WS_BORDER | ES_PASSWORD as u32 | ES_AUTOHSCROLL as u32,
        [9, 30, 232, 15],
        PASSWORD,
        0x81,
        "",
    );
    control(
        &mut words,
        WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
        [125, 54, 54, 15],
        IDOK,
        0x80,
        locale.text("解锁"),
    );
    control(
        &mut words,
        WS_TABSTOP,
        [187, 54, 54, 15],
        IDCANCEL,
        0x80,
        locale.text("取消"),
    );
    // The variable-length dialog template must start on a DWORD boundary.
    let mut aligned = vec![0u32; words.len().div_ceil(2)];
    std::ptr::copy_nonoverlapping(words.as_ptr(), aligned.as_mut_ptr().cast(), words.len());
    let mut result: Option<Password> = None;
    DialogBoxIndirectParamW(
        GetModuleHandleW(null()),
        aligned.as_ptr().cast(),
        hwnd,
        Some(on_message),
        std::ptr::addr_of_mut!(result) as LPARAM,
    );
    result
}

unsafe extern "system" fn on_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, lparam);
            SendDlgItemMessageW(hwnd, PASSWORD, EM_SETLIMITTEXT, 1024, 0);
            SetTimer(hwnd, 1, 1000, None);
            1
        }
        WM_COMMAND if wparam as u16 == IDOK as u16 => {
            let mut text = Zeroizing::new([0u16; 1025]);
            let size = GetDlgItemTextW(hwnd, PASSWORD, text.as_mut_ptr(), text.len() as i32);
            let value = String::from_utf16(&text[..size as usize])
                .ok()
                .and_then(|text| Password::new(text).ok());
            SetDlgItemTextW(hwnd, PASSWORD, w!(""));
            if value.is_some() {
                let result = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Option<Password>;
                *result = value;
                EndDialog(hwnd, 1);
            } else {
                let locale = native_locale::current();
                let mut warning = Vec::new();
                string(&mut warning, locale.text("备份密码须为 1–1024 字节。"));
                MessageBoxW(
                    hwnd,
                    warning.as_ptr(),
                    w!("DiskStudio · WinPE"),
                    MB_ICONWARNING,
                );
            }
            1
        }
        WM_COMMAND if wparam as u16 == IDCANCEL as u16 => {
            SetDlgItemTextW(hwnd, PASSWORD, w!(""));
            EndDialog(hwnd, 0);
            1
        }
        WM_CLOSE => {
            SetDlgItemTextW(hwnd, PASSWORD, w!(""));
            EndDialog(hwnd, 0);
            1
        }
        WM_TIMER if backup_secrets::status().is_none() => {
            SetDlgItemTextW(hwnd, PASSWORD, w!(""));
            EndDialog(hwnd, 0);
            1
        }
        _ => 0,
    }
}
