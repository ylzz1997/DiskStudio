//! Bounded, request/reply logical reads. Only opaque session/request identities
//! cross IPC; a failed transport cannot silently become zero-filled disk data.
use crate::{backup_archive::Source, image_browser::ReaderFactory};
use serde::Serialize;
use std::{
    collections::HashMap,
    io::{self, Read, Seek, SeekFrom},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant},
};
pub const BLOCK: usize = 64 * 1024;
const MAX_READERS: usize = 4;
#[derive(Clone, Serialize, Debug)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    Read {
        session: String,
        reader: String,
        request: String,
        offset: u64,
        length: usize,
    },
    ReaderClosed {
        session: String,
        reader: String,
    },
    Closed {
        session: String,
    },
}
type SendEvent = dyn Fn(Event) -> Result<(), String> + Send + Sync;
struct Pending {
    reader: String,
    request: String,
    length: usize,
    result: Option<Result<Vec<u8>, String>>,
}
#[derive(Default)]
struct State {
    readers: HashMap<String, Arc<AtomicBool>>,
    pending: Option<Pending>,
    fault: Option<String>,
}
pub struct Bridge {
    pub id: String,
    size: u64,
    send: Arc<SendEvent>,
    state: Mutex<State>,
    ready: Condvar,
    timeout: Duration,
}
fn registry() -> std::sync::MutexGuard<'static, HashMap<String, Weak<Bridge>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Weak<Bridge>>>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
fn registered(id: &str) -> Result<Arc<Bridge>, String> {
    registry()
        .get(id)
        .and_then(Weak::upgrade)
        .ok_or("镜像数据通道已关闭。".into())
}
impl Bridge {
    pub fn new(
        size: u64,
        send: impl Fn(Event) -> Result<(), String> + Send + Sync + 'static,
    ) -> Result<Arc<Self>, String> {
        Self::with_timeout(size, Arc::new(send), Duration::from_secs(60))
    }
    fn with_timeout(
        size: u64,
        send: Arc<SendEvent>,
        timeout: Duration,
    ) -> Result<Arc<Self>, String> {
        if !(1024 * 1024..=1u64 << 43).contains(&size) || size % 512 != 0 {
            return Err("镜像逻辑容量无效。".into());
        }
        let bridge = Arc::new(Self {
            id: uuid::Uuid::new_v4().to_string(),
            size,
            send,
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            timeout,
        });
        let mut r = registry();
        r.retain(|_, v| v.strong_count() > 0);
        if r.len() >= 4 {
            return Err("镜像数据通道数量已达上限，请关闭其他镜像会话。".into());
        }
        r.insert(bridge.id.clone(), Arc::downgrade(&bridge));
        Ok(bridge)
    }
    pub fn healthy(&self) -> Result<(), String> {
        match &self.state.lock().map_err(|_| "镜像数据通道状态异常")?.fault {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
    fn cancel(&self) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for c in s.readers.values() {
            c.store(true, Ordering::Relaxed);
        }
        self.ready.notify_all();
    }
    fn failed(&self, message: String) -> String {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.fault.get_or_insert_with(|| message.clone());
        self.ready.notify_all();
        message
    }
    fn request(
        &self,
        reader: &str,
        cancel: &AtomicBool,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("镜像读取已取消。".into());
        }
        if length == 0
            || length > BLOCK
            || offset
                .checked_add(length as u64)
                .is_none_or(|n| n > self.size)
        {
            return Err("镜像逻辑读取越界。".into());
        }
        let request = uuid::Uuid::new_v4().to_string();
        {
            let mut s = self.state.lock().map_err(|_| "镜像数据通道状态异常")?;
            if let Some(e) = &s.fault {
                return Err(e.clone());
            }
            if !s.readers.contains_key(reader) || s.pending.is_some() {
                return Err("镜像读取器失效或正在处理另一项请求。".into());
            }
            s.pending = Some(Pending {
                reader: reader.into(),
                request: request.clone(),
                length,
                result: None,
            });
        }
        if let Err(e) = (self.send)(Event::Read {
            session: self.id.clone(),
            reader: reader.into(),
            request: request.clone(),
            offset,
            length,
        }) {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).pending = None;
            return Err(self.failed(format!("镜像数据通道中断，请重新打开镜像：{e}")));
        }
        let start = Instant::now();
        let mut s = self.state.lock().map_err(|_| "镜像数据通道状态异常")?;
        loop {
            let pending = s
                .pending
                .as_mut()
                .filter(|p| p.request == request)
                .ok_or("镜像读取请求已失效")?;
            if cancel.load(Ordering::Relaxed) {
                s.pending = None;
                return Err("镜像读取已取消。".into());
            }
            if let Some(result) = pending.result.take() {
                s.pending = None;
                if let Err(e) = &result {
                    s.fault
                        .get_or_insert_with(|| format!("镜像解码失败，请重新打开来源：{e}"));
                }
                return result;
            }
            if let Some(e) = s.fault.clone() {
                s.pending = None;
                return Err(e);
            }
            if start.elapsed() >= self.timeout {
                s.pending = None;
                let message = "镜像解码响应超时，请重新打开镜像。".to_string();
                s.fault = Some(message.clone());
                return Err(message);
            }
            s = self
                .ready
                .wait_timeout(s, Duration::from_millis(100))
                .map_err(|_| "镜像读取等待异常")?
                .0;
        }
    }
    fn reply(&self, request: &str, result: Result<Vec<u8>, String>) -> Result<(), String> {
        let mut s = self.state.lock().map_err(|_| "镜像数据通道状态异常")?;
        let p = s
            .pending
            .as_mut()
            .filter(|p| p.request == request)
            .ok_or("镜像读取响应已过期")?;
        if p.result.is_some() {
            return Err("镜像读取响应不能重复提交".into());
        }
        let result = match result {
            Ok(b) if b.len() == p.length => Ok(b),
            Ok(_) => Err("镜像解码返回长度不符；不会补零或使用部分数据。".into()),
            Err(e) => Err(e.chars().take(512).collect()),
        };
        p.result = Some(result);
        self.ready.notify_all();
        Ok(())
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = (self.send)(Event::Closed {
            session: self.id.clone(),
        });
    }
}
pub struct Factory(pub Arc<Bridge>);
impl ReaderFactory for Factory {
    fn reader(&self) -> Result<Box<dyn Source>, String> {
        self.0.healthy()?;
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut s = self.0.state.lock().map_err(|_| "镜像读取器状态异常")?;
        if s.readers.len() >= MAX_READERS {
            return Err("镜像独立读取器数量超过上限".into());
        }
        s.readers.insert(id.clone(), cancel.clone());
        Ok(Box::new(Reader {
            bridge: self.0.clone(),
            id,
            cancel,
            pos: 0,
            cache_at: 0,
            cache: vec![],
            read_bytes: 0,
        }))
    }
}
struct Reader {
    bridge: Arc<Bridge>,
    id: String,
    cancel: Arc<AtomicBool>,
    pos: u64,
    cache_at: u64,
    cache: Vec<u8>,
    read_bytes: u64,
}
impl Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("镜像读取已取消"));
        }
        self.bridge.healthy().map_err(io::Error::other)?;
        if out.is_empty() || self.pos == self.bridge.size {
            return Ok(0);
        }
        if self.cache.is_empty()
            || self.pos < self.cache_at
            || self.pos >= self.cache_at + self.cache.len() as u64
        {
            let at = self.pos / BLOCK as u64 * BLOCK as u64;
            let length = (self.bridge.size - at).min(BLOCK as u64) as usize;
            self.read_bytes = self
                .read_bytes
                .checked_add(length as u64)
                .ok_or_else(|| io::Error::other("镜像读取预算溢出"))?;
            if self.read_bytes > self.bridge.size * 16 + 64 * 1024 * 1024 {
                return Err(io::Error::other(self.bridge.failed(
                    "本次镜像读取超过资源预算，请重新打开镜像并缩小操作范围".into(),
                )));
            }
            self.cache = self
                .bridge
                .request(&self.id, &self.cancel, at, length)
                .map_err(io::Error::other)?;
            self.cache_at = at;
        }
        let within = (self.pos - self.cache_at) as usize;
        let n = out.len().min(self.cache.len() - within);
        out[..n].copy_from_slice(&self.cache[within..within + n]);
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for Reader {
    fn seek(&mut self, at: SeekFrom) -> io::Result<u64> {
        let pos = match at {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(n) => self.bridge.size as i128 + n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
        };
        if pos < 0 || pos > self.bridge.size as i128 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "镜像读取越界"));
        }
        self.pos = pos as u64;
        Ok(self.pos)
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        let mut s = self.bridge.state.lock().unwrap_or_else(|e| e.into_inner());
        s.readers.remove(&self.id);
        if s.pending.as_ref().is_some_and(|p| p.reader == self.id) {
            s.pending = None;
        }
        drop(s);
        let _ = (self.bridge.send)(Event::ReaderClosed {
            session: self.bridge.id.clone(),
            reader: self.id.clone(),
        });
    }
}
pub fn cancel_all() {
    let bridges: Vec<_> = registry().values().filter_map(Weak::upgrade).collect();
    for bridge in bridges {
        bridge.cancel();
    }
}
pub fn reply_bytes(frame: &[u8]) -> Result<(), String> {
    // Two binary UUIDs followed by an exact non-empty <=64KiB response.
    if frame.len() <= 32 || frame.len() > 32 + BLOCK {
        return Err("镜像响应帧长度无效".into());
    }
    let session = uuid::Uuid::from_slice(&frame[..16])
        .map_err(|_| "镜像会话标识无效")?
        .to_string();
    let request = uuid::Uuid::from_slice(&frame[16..32])
        .map_err(|_| "镜像请求标识无效")?
        .to_string();
    registered(&session)?.reply(&request, Ok(frame[32..].to_vec()))
}
pub fn reply_error(session: &str, request: &str, message: String) -> Result<(), String> {
    if message.len() > 4096 {
        return Err("镜像错误响应过长".into());
    }
    registered(session)?.reply(request, Err(message))
}

#[cfg(test)]
#[path = "image_bridge_tests.rs"]
mod tests;
