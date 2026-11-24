export const signatureTypes = [
  { id: "png", label: "PNG" },
  { id: "jpeg", label: "JPEG" },
  { id: "pdf", label: "PDF" },
  { id: "gif", label: "GIF" },
  { id: "bmp", label: "BMP", detail: "未压缩的 Windows BMP" },
  { id: "wav", label: "WAV", detail: "RIFF/WAVE；不支持 RF64" },
  { id: "webp", label: "WebP" },
  {
    id: "zip",
    label: "ZIP",
    detail: "单卷 Stored/Deflate；含常规 ZIP64，不含加密、多卷和自解压头",
  },
  {
    id: "docx",
    label: "DOCX",
    detail: "Word Open XML 包；不含旧版 DOC、加密或宏类型",
  },
  {
    id: "xlsx",
    label: "XLSX",
    detail: "Excel Open XML 包；不含旧版 XLS、加密或宏类型",
  },
  {
    id: "pptx",
    label: "PPTX",
    detail: "PowerPoint Open XML 包；不含旧版 PPT、加密或宏类型",
  },
  { id: "docm", label: "DOCM", detail: "按 Open XML 主部件识别启用宏的文档。预览仅显示内容。" },
  { id: "xlsm", label: "XLSM", detail: "按 Open XML 主部件识别启用宏的文档。预览仅显示内容。" },
  { id: "pptm", label: "PPTM", detail: "按 Open XML 主部件识别启用宏的文档。预览仅显示内容。" },
  { id: "odt", label: "ODT", detail: "按媒体类型和清单识别未加密的 OpenDocument 包。" },
  { id: "ods", label: "ODS", detail: "按媒体类型和清单识别未加密的 OpenDocument 包。" },
  { id: "odp", label: "ODP", detail: "按媒体类型和清单识别未加密的 OpenDocument 包。" },
  {
    id: "sevenZip",
    label: "7z",
    detail: "核对 7z 文件头 CRC 和容量。压缩内容和加密状态未验证。",
  },
  {
    id: "sqlite",
    label: "SQLite",
    detail: "核对 SQLite 主数据库容量和首页。WAL 单独保存，无法随此候选还原。",
  },
  { id: "mp4", label: "MP4", detail: "核对独立 MP4/MOV 的媒体块与采样表。" },
  { id: "mov", label: "MOV", detail: "核对独立 MP4/MOV 的媒体块与采样表。" },
  { id: "ogg", label: "Ogg", detail: "核对连续 Ogg 页的序列、包边界与 CRC。" },
  { id: "tiff", label: "TIFF", detail: "核对 TIFF 和 BigTIFF 的图像目录及条带或分块范围。" },
  { id: "ico", label: "ICO", detail: "核对图标目录及内嵌 PNG 或位图。" },
  { id: "cur", label: "CUR", detail: "核对光标目录、热点及内嵌 PNG 或位图。" },
  { id: "rar", label: "RAR", detail: "核对单卷 RAR4/5 的块头 CRC 和结束块。不含加密存档。" },
  { id: "avi", label: "AVI", detail: "核对单段 AVI 的媒体块和已有索引。" },
  { id: "aiff", label: "AIFF", detail: "核对 AIFF 音频的采样帧数和声音范围。" },
  { id: "aifc", label: "AIFF-C", detail: "核对 PCM、浮点及 A-law/μ-law AIFF-C 的声音范围。" },
  { id: "flac", label: "FLAC", detail: "按采样总数、帧顺序和帧 CRC 定位原生 FLAC 文件。" },
  { id: "midi", label: "MIDI", detail: "核对标准 MIDI 文件的音轨及结束事件。" },
  { id: "dsf", label: "DSF", detail: "核对 DSF 声道块、填充和可选 ID3 标签边界。" },
  { id: "dff", label: "DFF", detail: "核对未压缩 DSDIFF 的声音属性和数据范围。" },
] as const;
export type SignatureKind = (typeof signatureTypes)[number]["id"];
export const zipPackageKinds: SignatureKind[] = [
  "zip", "docx", "xlsx", "pptx", "docm", "xlsm", "pptm", "odt", "ods", "odp",
];
export type SignatureSettings = {
  kinds: SignatureKind[];
  customRange: boolean;
  start: string;
  end: string;
  freeOnly: boolean;
  sectorAligned: boolean;
  zipFragments: boolean;
  oggFragments: boolean;
  flacFragments: boolean;
  pngFragments: boolean;
};
export const defaultSignatureSettings: SignatureSettings = {
  kinds: signatureTypes.map((t) => t.id),
  customRange: false,
  start: "0",
  end: "",
  freeOnly: false,
  sectorAligned: false,
  zipFragments: false,
  oggFragments: false,
  flacFragments: false,
  pngFragments: false,
};
export function signatureRequest(
  settings: SignatureSettings,
  sourceSize: number,
) {
  if (!Number.isSafeInteger(sourceSize) || sourceSize <= 0)
    throw Error("请选择有效的扫描范围。");
  if (!settings.kinds.length) throw Error("请至少选择一种文件类型。");
  if (
    new Set(settings.kinds).size !== settings.kinds.length ||
    settings.kinds.some((k) => !signatureTypes.some((t) => t.id === k))
  )
    throw Error("文件类型重复或无效。");
  const parse = (value: string) => {
    if (!/^\d+(\.\d{1,9})?$/.test(value.trim()))
      throw Error("请输入非负 MiB 数值，最多 9 位小数。");
    const n = Math.round(Number(value) * 1024 ** 2);
    if (!Number.isSafeInteger(n)) throw Error("扫描位置过大。");
    return n;
  };
  const start = settings.customRange ? parse(settings.start) : 0;
  const end =
    settings.customRange && settings.end.trim()
      ? parse(settings.end)
      : sourceSize;
  if (start < 0 || start >= end || end > sourceSize)
    throw Error("开始位置须小于结束位置，且范围不能超出所选磁盘或分区。");
  return {
    kinds: settings.kinds,
    start,
    length: end === sourceSize ? null : end - start,
    ...(settings.freeOnly && { freeOnly: true }),
    ...(settings.sectorAligned && { sectorAligned: true }),
    ...(settings.zipFragments && settings.kinds.some((kind) => zipPackageKinds.includes(kind)) && {
      zipFragments: true,
    }),
    ...(settings.oggFragments && settings.kinds.includes("ogg") && { oggFragments: true }),
    ...(settings.flacFragments && settings.kinds.includes("flac") && { flacFragments: true }),
    ...(settings.pngFragments && settings.kinds.includes("png") && { pngFragments: true }),
  };
}
export function previewImageMime(name: string): string | null {
  const extension = name.split(".").pop()?.toLowerCase();
  return (
    (
      {
        png: "image/png",
        jpg: "image/jpeg",
        jpeg: "image/jpeg",
        gif: "image/gif",
        bmp: "image/bmp",
        webp: "image/webp",
        ico: "image/x-icon",
      } as Record<string, string>
    )[extension ?? ""] ?? null
  );
}

export function previewMediaMime(name: string): string | null {
  const dot = name.lastIndexOf(".");
  if (dot < 0) return null;
  const extension = name.slice(dot + 1).toLowerCase();
  return ({
    mp4: "video/mp4",
    mov: "video/quicktime",
    ogg: "application/ogg",
    wav: "audio/wav",
    flac: "audio/flac",
  } as Record<string, string>)[extension] ?? null;
}
