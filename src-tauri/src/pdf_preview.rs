//! Render a local PDF to a single PNG through Windows' in-memory PDF API.
use crate::raster_preview::{MAX_PNG_BYTES, MAX_RENDER_EDGE};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use windows::{
    core::Interface,
    Data::Pdf::{PdfDocument, PdfPageRenderOptions},
    Graphics::Imaging::BitmapEncoder,
    Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream},
    Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
};
use windows_future::IAsyncOperation;

struct Apartment;

impl Apartment {
    fn initialize() -> Result<Self, String> {
        // This runs on a blocking thread and owns one matching WinRT initialization.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(unavailable)?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

fn unavailable(_: windows::core::Error) -> String {
    "Windows 无法呈现此 PDF 页面。".into()
}

fn wait<T: Send + 'static>(
    deadline: Instant,
    register: impl FnOnce(Box<dyn FnOnce(windows::core::Result<T>) + Send>) -> windows::core::Result<()>,
    cancel: impl FnOnce() -> windows::core::Result<()>,
) -> Result<T, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    register(Box::new(move |result| {
        let _ = sender.send(result);
    }))
    .map_err(unavailable)?;
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result.map_err(unavailable),
        Err(_) => {
            let _ = cancel();
            Err("PDF 页面预览超时。".into())
        }
    }
}

pub(crate) fn render(bytes: &[u8], index: u32) -> Result<(u32, Vec<u8>), String> {
    if !bytes.starts_with(b"%PDF-")
        || !bytes[bytes.len().saturating_sub(4096)..]
            .windows(5)
            .any(|end| end == b"%%EOF")
    {
        return Err("PDF 文件缺少有效的文件头或结束标记。".into());
    }
    let _apartment = Apartment::initialize()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let input = InMemoryRandomAccessStream::new().map_err(unavailable)?;
    let writer = DataWriter::CreateDataWriter(&input.GetOutputStreamAt(0).map_err(unavailable)?)
        .map_err(unavailable)?;
    writer.WriteBytes(bytes).map_err(unavailable)?;
    let store = writer
        .StoreAsync()
        .and_then(|op| op.cast::<IAsyncOperation<u32>>())
        .map_err(unavailable)?;
    if wait(deadline, |done| store.when(done), || store.Cancel())? as usize != bytes.len() {
        return Err("PDF 内容未完整传入渲染器。".into());
    }
    writer.DetachStream().map_err(unavailable)?;
    input.Seek(0).map_err(unavailable)?;
    let open = PdfDocument::LoadFromStreamAsync(&input).map_err(unavailable)?;
    let document = wait(deadline, |done| open.when(done), || open.Cancel())?;
    if document.IsPasswordProtected().map_err(unavailable)? {
        return Err("加密 PDF 暂不支持分页预览。".into());
    }
    let count = document.PageCount().map_err(unavailable)?;
    if index >= count {
        return Err("预览页码超出范围。".into());
    }
    let page = document.GetPage(index).map_err(unavailable)?;
    let size = page.Size().map_err(unavailable)?;
    if !size.Width.is_finite()
        || !size.Height.is_finite()
        || size.Width <= 0.0
        || size.Height <= 0.0
        || size.Width.max(size.Height) > 64_000.0
    {
        return Err("PDF 页面尺寸超过预览限额。".into());
    }
    let scale = MAX_RENDER_EDGE as f32 / size.Width.max(size.Height);
    let options = PdfPageRenderOptions::new().map_err(unavailable)?;
    options
        .SetDestinationWidth(
            (size.Width * scale)
                .round()
                .clamp(1.0, MAX_RENDER_EDGE as f32) as u32,
        )
        .map_err(unavailable)?;
    options
        .SetDestinationHeight(
            (size.Height * scale)
                .round()
                .clamp(1.0, MAX_RENDER_EDGE as f32) as u32,
        )
        .map_err(unavailable)?;
    options
        .SetBitmapEncoderId(BitmapEncoder::PngEncoderId().map_err(unavailable)?)
        .map_err(unavailable)?;
    let output = InMemoryRandomAccessStream::new().map_err(unavailable)?;
    let render = page
        .RenderWithOptionsToStreamAsync(&output, &options)
        .map_err(unavailable)?;
    wait(deadline, |done| render.when(done), || render.Cancel())?;
    page.Close().map_err(unavailable)?;
    let length = output.Size().map_err(unavailable)?;
    if length == 0 || length > MAX_PNG_BYTES as u64 {
        return Err("页面预览图像无效或超出限额。".into());
    }
    let reader = DataReader::CreateDataReader(&output.GetInputStreamAt(0).map_err(unavailable)?)
        .map_err(unavailable)?;
    let load = reader
        .LoadAsync(length as u32)
        .and_then(|op| op.cast::<IAsyncOperation<u32>>())
        .map_err(unavailable)?;
    if u64::from(wait(deadline, |done| load.when(done), || load.Cancel())?) != length {
        return Err("PDF 预览图像读取不完整。".into());
    }
    let mut png = vec![0; length as usize];
    reader.ReadBytes(&mut png).map_err(unavailable)?;
    Ok((count, png))
}
