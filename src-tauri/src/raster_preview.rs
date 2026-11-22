//! Bound page rendering to an immutable, in-memory copy of a retained candidate.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use std::{
    io::Cursor,
    sync::{Arc, Mutex},
};
use tiff::{
    decoder::{Decoder, DecodingResult, Limits},
    tags::Tag,
    ColorType,
};

pub(crate) const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_RENDER_EDGE: u32 = 1280;
const MAX_TIFF_PAGES: u32 = 256;
const MAX_TIFF_PIXELS: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Pdf,
    Tiff,
}

impl Format {
    pub(crate) fn from_extension(extension: &str) -> Option<Self> {
        match extension {
            "pdf" => Some(Self::Pdf),
            "tif" | "tiff" => Some(Self::Tiff),
            _ => None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Page {
    index: u32,
    count: u32,
    png_base64: String,
}

#[derive(Serialize)]
pub(crate) struct Opened {
    id: String,
    #[serde(flatten)]
    page: Page,
}

#[derive(Clone)]
struct Snapshot {
    scan_id: String,
    id: String,
    format: Format,
    count: u32,
    bytes: Arc<[u8]>,
}

static ACTIVE_PREVIEW: Mutex<Option<Snapshot>> = Mutex::new(None);
static RENDERING: Mutex<()> = Mutex::new(());

pub(crate) fn open(scan_id: &str, bytes: &[u8], format: Format) -> Result<Opened, String> {
    let page = render(bytes, format, 0)?;
    let id = uuid::Uuid::new_v4().to_string();
    let snapshot = Snapshot {
        scan_id: scan_id.into(),
        id: id.clone(),
        format,
        count: page.count,
        bytes: Arc::from(bytes),
    };
    *ACTIVE_PREVIEW.lock().map_err(|_| "预览不可用。")? = Some(snapshot);
    Ok(Opened { id, page })
}

pub(crate) fn page(scan_id: &str, id: &str, index: u32) -> Result<Page, String> {
    let snapshot = ACTIVE_PREVIEW
        .lock()
        .map_err(|_| "预览不可用。")?
        .as_ref()
        .filter(|source| source.id == id && source.scan_id == scan_id)
        .ok_or("分页预览已失效。")?
        .clone();
    if index >= snapshot.count {
        return Err("预览页码超出范围。".into());
    }
    let result = render(&snapshot.bytes, snapshot.format, index)?;
    if result.count != snapshot.count {
        return Err("预览页数与打开时不一致。".into());
    }
    Ok(result)
}

pub(crate) fn discard(id: &str) {
    if let Ok(mut preview) = ACTIVE_PREVIEW.lock() {
        if preview.as_ref().is_some_and(|source| source.id == id) {
            *preview = None;
        }
    }
}

fn render(bytes: &[u8], format: Format, index: u32) -> Result<Page, String> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err("分页预览需要不超过 8 MiB 的完整文件。".into());
    }
    let _rendering = RENDERING
        .try_lock()
        .map_err(|_| "请等待当前页面完成预览。")?;
    let (count, png) = match format {
        #[cfg(windows)]
        Format::Pdf => crate::pdf_preview::render(bytes, index)?,
        #[cfg(not(windows))]
        Format::Pdf => return Err("仅 Windows 支持 PDF 分页预览。".into()),
        Format::Tiff => render_tiff(bytes, index)?,
    };
    if png.len() > MAX_PNG_BYTES || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("页面预览图像无效或超出限额。".into());
    }
    Ok(Page {
        index,
        count,
        png_base64: STANDARD.encode(png),
    })
}

fn tiff_decoder(bytes: &[u8]) -> Result<Decoder<Cursor<&[u8]>>, String> {
    let mut limits = Limits::default();
    limits.decoding_buffer_size = 64 * 1024 * 1024;
    limits.intermediate_buffer_size = 16 * 1024 * 1024;
    limits.ifd_value_size = 512 * 1024;
    Decoder::new(Cursor::new(bytes))
        .map(|decoder| decoder.with_limits(limits))
        .map_err(decode_error)
}

fn decode_error(_: tiff::TiffError) -> String {
    "无法解码此 TIFF 页面。".into()
}

fn render_tiff(bytes: &[u8], index: u32) -> Result<(u32, Vec<u8>), String> {
    let mut decoder = tiff_decoder(bytes)?;
    let mut count = 1;
    while decoder.more_images() {
        if count == MAX_TIFF_PAGES {
            return Err("TIFF 页数超过预览限额。".into());
        }
        decoder.next_image().map_err(decode_error)?;
        count += 1;
    }
    if index >= count {
        return Err("预览页码超出范围。".into());
    }
    decoder
        .seek_to_image(index as usize)
        .map_err(decode_error)?;
    let (width, height) = decoder.dimensions().map_err(decode_error)?;
    if width == 0
        || height == 0
        || width.max(height) > 16_384
        || u64::from(width) * u64::from(height) > MAX_TIFF_PIXELS
    {
        return Err("TIFF 页面尺寸超过预览限额。".into());
    }
    if decoder
        .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
        .map_err(decode_error)?
        .unwrap_or(1)
        != 1
    {
        return Err("TIFF 页面使用暂不支持的分平面像素布局。".into());
    }
    let orientation = decoder
        .find_tag_unsigned::<u16>(Tag::Orientation)
        .map_err(decode_error)?
        .unwrap_or(1);
    if !(1..=8).contains(&orientation) {
        return Err("TIFF 页面方向无效。".into());
    }
    let (bits, color) = match decoder.colortype().map_err(decode_error)? {
        ColorType::Gray(bits @ (1 | 2 | 4 | 8 | 16)) => (bits, png::ColorType::Grayscale),
        ColorType::RGB(bits @ (8 | 16)) => (bits, png::ColorType::Rgb),
        ColorType::RGBA(bits @ (8 | 16)) => (bits, png::ColorType::Rgba),
        _ => return Err("TIFF 页面使用暂不支持的颜色类型。".into()),
    };
    let samples = color.samples();
    let associated_alpha = color == png::ColorType::Rgba
        && decoder
            .find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)
            .map_err(decode_error)?
            .is_some_and(|extra| extra.as_slice() == [1]);
    let (width, height) = (width as usize, height as usize);
    let row_bytes = (width * samples * usize::from(bits)).div_ceil(8);
    let pixels = decoder.read_image().map_err(decode_error)?;
    match &pixels {
        DecodingResult::U8(data) if bits <= 8 && data.len() == row_bytes * height => {}
        DecodingResult::U16(data) if bits == 16 && data.len() * 2 == row_bytes * height => {}
        _ => return Err("TIFF 页面像素布局与样本大小不一致。".into()),
    }
    let (oriented_w, oriented_h) = if orientation > 4 {
        (height, width)
    } else {
        (width, height)
    };
    let scale = oriented_w.max(oriented_h).max(MAX_RENDER_EDGE as usize);
    let (out_w, out_h) = (
        (oriented_w * MAX_RENDER_EDGE as usize / scale).max(1),
        (oriented_h * MAX_RENDER_EDGE as usize / scale).max(1),
    );
    let mut raw = Vec::with_capacity(out_w * out_h * samples);
    for row in 0..out_h {
        for col in 0..out_w {
            let (u, v) = (col * oriented_w / out_w, row * oriented_h / out_h);
            let (x, y) = source_coords(u, v, width, height, orientation);
            let sample = (y * width + x) * samples;
            let mut pixel = [0; 4];
            match &pixels {
                DecodingResult::U8(data) if bits < 8 => {
                    let bit = x * usize::from(bits);
                    let mask = (1u16 << bits) - 1;
                    let value = u16::from(
                        data[y * row_bytes + bit / 8] >> (8 - usize::from(bits) - bit % 8),
                    ) & mask;
                    pixel[0] = (value * 255 / mask) as u8;
                }
                DecodingResult::U8(data) => {
                    pixel[..samples].copy_from_slice(&data[sample..sample + samples])
                }
                DecodingResult::U16(data) => {
                    for (out, &value) in pixel.iter_mut().zip(&data[sample..sample + samples]) {
                        *out = ((u32::from(value) + 128) / 257) as u8;
                    }
                }
                _ => unreachable!("decoded sample type was checked"),
            }
            if associated_alpha {
                let alpha = u16::from(pixel[3]);
                for value in &mut pixel[..3] {
                    *value = (u16::from(*value) * 255)
                        .checked_div(alpha)
                        .unwrap_or(0)
                        .min(255) as u8;
                }
            }
            raw.extend_from_slice(&pixel[..samples]);
        }
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, out_w as u32, out_h as u32);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|_| "无法编码 TIFF 预览。")?;
    writer
        .write_image_data(&raw)
        .map_err(|_| "无法编码 TIFF 预览。")?;
    writer.finish().map_err(|_| "无法完成 TIFF 预览。")?;
    Ok((count, png))
}

fn source_coords(x: usize, y: usize, w: usize, h: usize, orientation: u16) -> (usize, usize) {
    match orientation {
        1 => (x, y),
        2 => (w - 1 - x, y),
        3 => (w - 1 - x, h - 1 - y),
        4 => (x, h - 1 - y),
        5 => (y, x),
        6 => (y, h - 1 - x),
        7 => (w - 1 - y, h - 1 - x),
        8 => (w - 1 - y, x),
        _ => unreachable!("orientation was checked"),
    }
}

#[cfg(test)]
#[path = "../../tests/fixtures/paged_preview.rs"]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut reader = png::Decoder::new(Cursor::new(png)).read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
        let image = reader.next_frame(&mut buffer).unwrap();
        buffer.truncate(image.buffer_size());
        (image.width, image.height, buffer)
    }

    #[test]
    fn previews_both_classic_tiff_pages_and_a_tiled_bigtiff() {
        let (count, first) = render_tiff(fixtures::TIFF, 0).unwrap();
        let (later_count, second) = render_tiff(fixtures::TIFF, 1).unwrap();
        assert_eq!((count, later_count), (2, 2));
        assert_ne!(pixels(&first), pixels(&second));
        let (count, big) = render_tiff(fixtures::BIG_TIFF, 0).unwrap();
        assert_eq!(count, 1);
        assert!(!pixels(&big).2.is_empty());
        assert!(render_tiff(fixtures::TIFF, 2).is_err());
    }

    #[test]
    fn orientation_is_applied_before_encoding_the_page() {
        use tiff::encoder::{colortype::Gray8, TiffEncoder};
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = TiffEncoder::new(&mut buffer).unwrap();
        let mut image = encoder.new_image::<Gray8>(3, 2).unwrap();
        image.encoder().write_tag(Tag::Orientation, 6u16).unwrap();
        image.write_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        let (_, rotated) = render_tiff(buffer.get_ref(), 0).unwrap();
        assert_eq!(pixels(&rotated), (2, 3, vec![4, 1, 5, 2, 6, 3]));
    }

    #[test]
    fn page_access_binds_snapshot_to_scan_and_discarded_id() {
        let mut source = fixtures::TIFF.to_vec();
        let opened = open("scan", &source, Format::Tiff).unwrap();
        source.fill(0);
        assert!(page("different scan", &opened.id, 1).is_err());
        assert!(page("scan", &opened.id, 2).is_err());
        let second = page("scan", &opened.id, 1).unwrap();
        assert_eq!((second.index, second.count), (1, 2));
        assert_ne!(opened.page.png_base64, second.png_base64);
        discard("different id");
        assert!(page("scan", &opened.id, 0).is_ok());
        discard(&opened.id);
        assert!(page("scan", &opened.id, 0).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn pdf_renders_actual_page_count_orientation_and_content() {
        let bytes = fixtures::pdf();
        let (first_count, first) = crate::pdf_preview::render(&bytes, 0).unwrap();
        let (second_count, second) = crate::pdf_preview::render(&bytes, 1).unwrap();
        let (w1, h1, p1) = pixels(&first);
        let (w2, h2, p2) = pixels(&second);
        assert_eq!((first_count, second_count), (2, 2));
        assert!(w1 > h1 && h2 > w2 && w1 <= MAX_RENDER_EDGE && h2 <= MAX_RENDER_EDGE);
        assert_ne!(p1[..3], p2[..3]);
        assert!(crate::pdf_preview::render(&bytes, 2).is_err());
        assert!(crate::pdf_preview::render(b"%PDF-1.7\n", 0).is_err());
    }
}
