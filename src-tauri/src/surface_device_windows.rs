//! Read-only raw device I/O for surface scans. All requests drain before their
//! buffer, OVERLAPPED, event, or file handle can be released.
use super::{open_device_file, verify_device, verify_unbuffered_alignment, Resolved};
use crate::surface_scan::{ScanSource, TransferError, TransferLimit};
use std::{
    fs::File,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{
        ReadFile, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
    System::{
        Ioctl::{StorageAdapterProperty, IOCTL_STORAGE_QUERY_PROPERTY},
        SystemInformation::{GetSystemInfo, SYSTEM_INFO},
        Threading::{CreateEventW, ResetEvent, WaitForSingleObject},
        IO::{
            CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED, OVERLAPPED_0,
            OVERLAPPED_0_0,
        },
    },
};

pub(crate) struct SurfaceReader {
    file: File,
    event: OwnedHandle,
    timeout: Option<Duration>,
}

struct Pending<'a> {
    file: &'a File,
    request: &'a mut OVERLAPPED,
    outstanding: bool,
}

impl Pending<'_> {
    fn result(&mut self, wait: bool) -> Result<u32, TransferError> {
        let mut bytes = 0;
        // SAFETY: this is the same retained handle and OVERLAPPED used to submit
        // the operation. The buffer remains borrowed until the operation drains.
        if unsafe {
            GetOverlappedResult(
                self.file.as_raw_handle(),
                self.request,
                &mut bytes,
                wait.into(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            self.outstanding = error.raw_os_error() == Some(ERROR_IO_INCOMPLETE as i32);
            return Err(TransferError::Io(error));
        }
        self.outstanding = false;
        Ok(bytes)
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.outstanding {
            // SAFETY: cancel this unique operation only. Cancellation can race
            // with completion or be delayed by a driver, so always wait for the
            // terminal result before permitting the request storage to drop.
            unsafe { CancelIoEx(self.file.as_raw_handle(), self.request) };
            let _ = self.result(true);
        }
    }
}

impl SurfaceReader {
    fn execute(
        &self,
        offset: u64,
        cancel: Option<&AtomicBool>,
        submit: impl FnOnce(*mut OVERLAPPED) -> i32,
    ) -> Result<usize, TransferError> {
        let event = self.event.as_raw_handle();
        // SAFETY: this manual-reset event belongs exclusively to this reader.
        if unsafe { ResetEvent(event) } == 0 {
            return Err(TransferError::Io(io::Error::last_os_error()));
        }
        let mut request = OVERLAPPED {
            hEvent: event,
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: offset as u32,
                    OffsetHigh: (offset >> 32) as u32,
                },
            },
            ..OVERLAPPED::default()
        };
        let mut pending = Pending {
            file: &self.file,
            request: &mut request,
            outstanding: false,
        };
        let started = Instant::now();
        if submit(pending.request) != 0 {
            return pending.result(false).map(|bytes| bytes as usize);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(TransferError::Io(error));
        }
        pending.outstanding = true;
        let Some(cancel) = cancel else {
            return pending.result(true).map(|bytes| bytes as usize);
        };

        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(TransferError::Canceled);
            }
            let remaining = self
                .timeout
                .map(|limit| limit.saturating_sub(started.elapsed()));
            if remaining.is_some_and(|time| time.is_zero()) {
                return Err(TransferError::TimedOut);
            }
            let wait_ms = remaining.map_or(50, |time| time.as_millis().clamp(1, 50) as u32);
            // SAFETY: the event is retained and signals only this operation.
            match unsafe { WaitForSingleObject(event, wait_ms) } {
                WAIT_OBJECT_0 => return pending.result(false).map(|bytes| bytes as usize),
                WAIT_TIMEOUT => continue,
                _ => return Err(TransferError::Io(io::Error::last_os_error())),
            }
        }
    }

    fn control(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, String> {
        // SAFETY: these are non-overlapping borrowed slices of fixed IOCTL data.
        // execute waits for completion before returning the borrows to the caller.
        self.execute(0, None, |request| unsafe {
            DeviceIoControl(
                self.file.as_raw_handle(),
                code,
                if input.is_empty() {
                    null()
                } else {
                    input.as_ptr().cast()
                },
                input.len() as u32,
                if output.is_empty() {
                    null_mut()
                } else {
                    output.as_mut_ptr().cast()
                },
                output.len() as u32,
                null_mut(),
                request,
            )
        })
        .map_err(|error| match error {
            TransferError::Io(error) => format!("Windows 访问失败：{error}"),
            _ => "无法核对检测设备属性。".into(),
        })
    }
}

impl ScanSource for SurfaceReader {
    fn read_at(
        &mut self,
        offset: u64,
        buffer: &mut [u8],
        cancel: &AtomicBool,
    ) -> Result<usize, TransferError> {
        // SAFETY: the scanner supplies an aligned, bounded buffer. It remains
        // exclusively borrowed until the precise operation has drained.
        self.execute(offset, Some(cancel), |request| unsafe {
            ReadFile(
                self.file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                null_mut(),
                request,
            )
        })
    }
}

pub(crate) fn surface_scan_device(
    resolved: &Resolved,
    timeout_seconds: Option<u16>,
) -> Result<(SurfaceReader, TransferLimit), String> {
    let file = open_device_file(
        resolved,
        false,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED,
    )?;
    // SAFETY: create an unnamed, unshared manual-reset event with no security override.
    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if event.is_null() {
        return Err(format!("无法创建读取事件：{}", io::Error::last_os_error()));
    }
    // SAFETY: the successful creation returned an owned kernel handle.
    let device = SurfaceReader {
        file,
        event: unsafe { OwnedHandle::from_raw_handle(event) },
        timeout: timeout_seconds.map(|seconds| Duration::from_secs(seconds.into())),
    };
    verify_device(resolved, |code, input, output| {
        device.control(code, input, output)
    })?;
    verify_unbuffered_alignment(resolved, |code, input, output| {
        device.control(code, input, output)
    })?;

    let mut query = [0; 12];
    query[..4].copy_from_slice(&StorageAdapterProperty.to_le_bytes());
    let mut adapter = [0; 64];
    let invalid = || "无法确认坏道检测的单次传输上限。".to_owned();
    let size = device
        .control(IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut adapter)
        .map_err(|_| invalid())?;
    let field = |offset| u32::from_le_bytes(adapter[offset..offset + 4].try_into().unwrap());
    if size < 20
        || size > adapter.len()
        || !(20..=size).contains(&(field(4) as usize))
        || field(0) < 20
    {
        return Err(invalid());
    }
    // SAFETY: GetSystemInfo populates a writable system information structure.
    let mut system: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut system) };
    let limit = TransferLimit::from_adapter(field(8), field(12), field(16), system.dwPageSize)?;
    Ok((device, limit))
}
