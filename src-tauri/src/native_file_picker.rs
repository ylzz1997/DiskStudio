//! Native file selection for Windows environments without the shell item dialog.
use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf, ptr::null};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, GetSaveFileNameW, OFN_DONTADDTORECENT,
        OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_NODEREFERENCELINKS,
        OFN_NOTESTFILECREATE, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    },
};

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

pub(crate) fn choose(
    owner: HWND,
    title: &str,
    filter: &str,
    suggested: Option<&str>,
    extension: Option<&str>,
    open_existing: bool,
) -> Result<Option<PathBuf>, String> {
    let mut name = vec![0u16; 32768];
    if let Some(suggested) = suggested {
        let suggestion = wide(suggested);
        name.get_mut(..suggestion.len())
            .ok_or("文件名无效")?
            .copy_from_slice(&suggestion);
    }
    let title = wide(title);
    let filter = wide(filter);
    let extension = extension.map(wide);
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: name.as_mut_ptr(),
        nMaxFile: name.len() as u32,
        lpstrTitle: title.as_ptr(),
        lpstrDefExt: extension.as_ref().map_or(null(), |value| value.as_ptr()),
        Flags: OFN_PATHMUSTEXIST
            | OFN_NOCHANGEDIR
            | OFN_NODEREFERENCELINKS
            | OFN_HIDEREADONLY
            | OFN_DONTADDTORECENT
            | if open_existing {
                OFN_FILEMUSTEXIST
            } else {
                OFN_NOTESTFILECREATE
            },
        ..Default::default()
    };
    let selected = unsafe {
        if open_existing {
            GetOpenFileNameW(&mut dialog)
        } else {
            GetSaveFileNameW(&mut dialog)
        }
    };
    if selected == 0 {
        return match unsafe { CommDlgExtendedError() } {
            0 => Ok(None),
            code => Err(format!(
                "{} (0x{code:x})",
                crate::native_locale::current().text("文件选择窗口未能打开。")
            )),
        };
    }
    let end = name
        .iter()
        .position(|&unit| unit == 0)
        .ok_or("文件路径长度无效")?;
    Ok(Some(PathBuf::from(OsString::from_wide(&name[..end]))))
}
