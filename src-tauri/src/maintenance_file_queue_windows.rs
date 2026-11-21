//! Independent submission queues for ordinary files. Each worker services one
//! private completion port, so queue depth can exceed the number of threads.
use crate::{
    maintenance_io::{BenchmarkReadQueue, ReadWorker},
    surface_scan::TransferError,
};
use std::{
    cell::{Cell, UnsafeCell},
    fs::File,
    io,
    mem::{size_of, MaybeUninit},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::ReadFile,
    System::{
        Threading::{CreateEventW, ResetEvent, WaitForSingleObject, INFINITE},
        IO::{
            CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
            OVERLAPPED_0, OVERLAPPED_0_0,
        },
    },
};

#[repr(C, align(4096))]
struct Page {
    _bytes: [u8; 4096],
}

struct Buffer {
    pages: Vec<UnsafeCell<MaybeUninit<Page>>>,
    event: OwnedHandle,
}

impl Buffer {
    fn new(block: usize) -> Result<Self, String> {
        let count = block / size_of::<Page>();
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|error| format!("无法分配文件测速缓冲区：{error}"))?;
        pages.resize_with(count, || UnsafeCell::new(MaybeUninit::uninit()));
        // SAFETY: create a private manual-reset event for this buffer only.
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: the successful call returned a new owned event handle.
        Ok(Self {
            pages,
            event: unsafe { OwnedHandle::from_raw_handle(event) },
        })
    }
}

pub(super) struct FileQueue {
    file: File,
    port: OwnedHandle,
    buffers: Vec<Buffer>,
}

impl FileQueue {
    pub(super) fn new(file: File, depth: u16, block: u32) -> Result<Self, String> {
        let buffers = (0..depth)
            .map(|_| Buffer::new(block as usize))
            .collect::<Result<_, _>>()?;
        // SAFETY: file was independently opened for overlapped reads and has
        // no prior completion port association. This port has one consumer.
        let port = unsafe { CreateIoCompletionPort(file.as_raw_handle(), null_mut(), 0, 1) };
        if port.is_null() {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: successful creation returned a new owned completion port.
        Ok(Self {
            file,
            port: unsafe { OwnedHandle::from_raw_handle(port) },
            buffers,
        })
    }
}

#[repr(C)]
struct Slot {
    request: UnsafeCell<OVERLAPPED>,
    buffer: Buffer,
    started: Cell<Option<Instant>>,
}

struct PendingQueue {
    file: File,
    port: OwnedHandle,
    slots: Box<[Slot]>,
    pending: usize,
}

impl PendingQueue {
    fn submit(&mut self, index: usize, worker: &ReadWorker<'_>) -> Result<(), String> {
        let Some(offset) = worker.next_offset() else {
            return Ok(());
        };
        let slot = &self.slots[index];
        debug_assert!(slot.started.get().is_none());
        let event = slot.buffer.event.as_raw_handle();
        // SAFETY: no request is pending for this slot. Only this worker resets
        // the event and prepares this OVERLAPPED before the kernel borrows it.
        if unsafe { ResetEvent(event) } == 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let request = OVERLAPPED {
            hEvent: event,
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: offset as u32,
                    OffsetHigh: (offset >> 32) as u32,
                },
            },
            ..OVERLAPPED::default()
        };
        let started = worker.issued();
        // SAFETY: the stable slot owns a page-aligned writable buffer and a
        // distinct OVERLAPPED until its completion packet has been consumed.
        // Buffer contents are never read by Rust.
        let result = unsafe {
            slot.request.get().write(request);
            ReadFile(
                self.file.as_raw_handle(),
                slot.buffer.pages.as_ptr().cast_mut().cast(),
                (slot.buffer.pages.len() * size_of::<Page>()) as u32,
                null_mut(),
                slot.request.get(),
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                worker.completed(Err(TransferError::Io(error)), started);
                return Ok(());
            }
        }
        // The default completion mode also posts a packet for inline success.
        slot.started.set(Some(started));
        self.pending += 1;
        Ok(())
    }

    fn consume(
        &mut self,
        timeout: u32,
    ) -> Result<Option<(usize, Instant, Result<usize, TransferError>)>, String> {
        let mut request = null_mut();
        let mut count = 0;
        let mut key = 0;
        // SAFETY: the port is live with one consumer and receives only native
        // read completions. All three output variables remain valid.
        let result = unsafe {
            GetQueuedCompletionStatus(
                self.port.as_raw_handle(),
                &mut count,
                &mut key,
                &mut request,
                timeout,
            )
        };
        let error = (result == 0).then(io::Error::last_os_error);
        if request.is_null() {
            return match error {
                Some(error) if error.raw_os_error() == Some(WAIT_TIMEOUT as i32) => Ok(None),
                _ => Err("文件测速完成端口未返回请求。".into()),
            };
        }
        let address = request as usize;
        let base = self.slots.as_ptr() as usize;
        // OVERLAPPED is the first field of every stable repr(C) slot.
        let index = address
            .checked_sub(base)
            .filter(|offset| offset % size_of::<Slot>() == 0)
            .map(|offset| offset / size_of::<Slot>())
            .filter(|&index| index < self.slots.len() && key == 0)
            .ok_or("文件测速完成端口返回了未知的请求。")?;
        let started = self.slots[index]
            .started
            .take()
            .ok_or("文件测速收到重复完成通知。")?;
        self.pending -= 1;
        let read = match error {
            None => Ok(count as usize),
            Some(error) if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) => {
                Err(TransferError::Canceled)
            }
            Some(error) => Err(TransferError::Io(error)),
        };
        Ok(Some((index, started, read)))
    }

    fn cancel(&self) {
        // SAFETY: no other worker has this independently reopened file handle.
        // Only requests on this handle are eligible; each must still drain.
        unsafe { CancelIoEx(self.file.as_raw_handle(), null()) };
    }
}

impl Drop for PendingQueue {
    fn drop(&mut self) {
        if self.pending == 0 {
            return;
        }
        self.cancel();
        while self.pending != 0 {
            if self.consume(INFINITE).is_err() {
                // A broken completion port is exceptional. The private event
                // still establishes that the native buffer borrow has ended.
                for slot in self
                    .slots
                    .iter()
                    .filter(|slot| slot.started.get().is_some())
                {
                    // SAFETY: the event remains live until the slot is released.
                    if unsafe { WaitForSingleObject(slot.buffer.event.as_raw_handle(), INFINITE) }
                        != WAIT_OBJECT_0
                    {
                        // Preserve backing allocations if the driver and event
                        // fail to establish completion. Never free pending I/O.
                        std::mem::forget(std::mem::take(&mut self.slots));
                        return;
                    }
                }
                break;
            }
        }
    }
}

impl BenchmarkReadQueue for FileQueue {
    fn capacity(&self) -> u16 {
        self.buffers.len() as u16
    }

    fn run(self, worker: &ReadWorker<'_>) -> Result<(), String> {
        let mut queue = PendingQueue {
            file: self.file,
            port: self.port,
            slots: self
                .buffers
                .into_iter()
                .map(|buffer| Slot {
                    request: UnsafeCell::new(OVERLAPPED::default()),
                    buffer,
                    started: Cell::new(None),
                })
                .collect(),
            pending: 0,
        };
        for index in 0..queue.slots.len() {
            queue.submit(index, worker)?;
        }
        let mut cancel_sent = false;
        while queue.pending != 0 {
            if !cancel_sent && worker.stopping() {
                queue.cancel();
                cancel_sent = true;
            }
            if let Some((index, started, result)) = queue.consume(25)? {
                worker.completed(result, started);
                queue.submit(index, worker)?;
            }
        }
        Ok(())
    }
}
