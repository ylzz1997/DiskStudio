// Hosted sparse VMDK. Read only the explicitly selected File;
// embedded extent names are checked as metadata, never resolved as paths.
import {
  logicalLimit,
  fileFactor,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
import { parseVmdkDescriptor } from "./vmdkDescriptor";
export const vmdkBudget = (stream: ImageReadMode = false) => ({
  stream,
  byteLimit: 160 * 1024 ** 2,
  callLimit: 100000,
  fileBytes: 0,
  calls: 0,
  metaBytes: 0,
  mapBytes: 0,
});
export type VmdkBudget = ReturnType<typeof vmdkBudget>;
type ReadParent = LogicalImage["read"];
const MiB = 1024 ** 2,
  TABLE_LIMIT = 32 * MiB;
const dv = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
const u32 = (b: Uint8Array, at: number) => dv(b).getUint32(at, true);
const u64 = (b: Uint8Array, at: number) => {
  const n = dv(b).getBigUint64(at, true);
  if (n > BigInt(Number.MAX_SAFE_INTEGER))
    throw Error("VMDK 数值超出安全范围。");
  return Number(n);
};
const same = (a: Uint8Array, b: Uint8Array) =>
  a.length === b.length && a.every((v, i) => v === b[i]);
const align = (n: number) => Math.ceil(n / 512) * 512;
type Span = { at: number; end: number };
type Inflate = (bytes: Uint8Array, expected: number) => Promise<Uint8Array>;
export function vmdkRange(at: number, length: number, size: number) {
  if (
    !Number.isSafeInteger(at) ||
    !Number.isSafeInteger(length) ||
    at < 0 ||
    length < 1 ||
    at > size - length
  )
    throw Error("VMDK 读取范围越界。");
}
export function chargeVmdkTables(budget: VmdkBudget, length: number) {
  budget.mapBytes += length;
  if (budget.mapBytes > TABLE_LIMIT)
    throw Error("VMDK 粒度映射超过 32 MiB 上限。");
}

export function vmdkFileReads(
  file: Pick<ImageFile, "size" | "slice">,
  budget: VmdkBudget,
) {
  if (!Number.isSafeInteger(file.size) || file.size < 512 || file.size % 512)
    throw Error("VMDK 文件长度无效或未按扇区对齐。");
  if (budget.stream) {
    budget.byteLimit += file.size * fileFactor(budget.stream);
    budget.callLimit += (file.size / 512) * fileFactor(budget.stream);
  }
  const physical = async (at: number, length: number) => {
    vmdkRange(at, length, file.size);
    budget.fileBytes += length;
    if (
      length > TABLE_LIMIT ||
      budget.fileBytes > budget.byteLimit ||
      ++budget.calls > budget.callLimit
    )
      throw Error("VMDK 分区表读取超过资源上限。");
    const b = new Uint8Array(await file.slice(at, at + length).arrayBuffer());
    if (b.length !== length) throw Error("VMDK 文件读取不完整。");
    return b;
  };
  const saved: { at: number; bytes: Uint8Array }[] = [];
  const capture = async (at: number, length: number) => {
    budget.metaBytes += length;
    if (budget.metaBytes > 66 * MiB)
      throw Error("VMDK 元数据超过 66 MiB 上限。");
    const bytes = await physical(at, length);
    saved.push({ at, bytes });
    return bytes;
  };
  return {
    physical,
    capture,
    async verify() {
      for (const { at, bytes } of saved)
        if (!same(bytes, await physical(at, bytes.length)))
          throw Error(
            "读取期间 VMDK 索引、描述符或压缩标记发生变化，请重新打开离线副本。",
          );
    },
  };
}
function disjoint(spans: Span[]) {
  spans.sort((a, b) => a.at - b.at);
  for (let i = 1; i < spans.length; i++)
    if (spans[i].at < spans[i - 1].end) throw Error("VMDK 元数据区域重叠。");
}
function overlaps(spans: Span[], at: number, end: number) {
  let lo = 0,
    hi = spans.length;
  while (lo < hi) {
    const m = (lo + hi) >>> 1;
    if (spans[m].at < end) lo = m + 1;
    else hi = m;
  }
  return lo > 0 && spans[lo - 1].end > at;
}
function header(bytes: Uint8Array, external = false) {
  if (u32(bytes, 0) !== 0x564d444b)
    throw Error("VMDK 稀疏文件头无效；请先选择主描述文件。");
  const version = u32(bytes, 4),
    flags = u32(bytes, 8),
    compressed = Boolean(flags & 0x10000);
  if (
    ![1, 2, 3].includes(version) ||
    (flags & ~0x30007) !== 0 ||
    Boolean(flags & 0x20000) !== compressed ||
    dv(bytes).getUint16(77, true) !== (compressed ? 1 : 0)
  )
    throw Error("VMDK 版本、压缩方式或功能标志暂不支持。");
  if (bytes[72] !== 0)
    throw Error(
      "VMDK 未正常关闭；请使用关闭后的离线副本，当前不检查修复活动镜像。",
    );
  if (
    flags & 1 &&
    !same(bytes.subarray(73, 77), new Uint8Array([10, 32, 13, 10]))
  )
    throw Error("VMDK 换行检测字段损坏。");
  const capacity = u64(bytes, 12),
    grain = u64(bytes, 20) * 512,
    size = capacity * 512;
  if (
    !Number.isSafeInteger(size) ||
    size < (external ? 512 : 4 * MiB) ||
    size > 2 ** 43
  )
    throw Error("VMDK 逻辑容量须为 4 MiB 至 8 TiB。");
  if (
    !Number.isSafeInteger(grain) ||
    grain < 8192 ||
    grain > 256 * MiB ||
    (grain & (grain - 1)) !== 0 ||
    (compressed && (grain > MiB || size % grain !== 0))
  )
    throw Error("VMDK 粒度暂不支持；压缩盘要求完整的 8 KiB 至 1 MiB 数据块。");
  if (u32(bytes, 44) !== 512) throw Error("VMDK 当前要求每张粒度表有 512 项。");
  const overhead = u64(bytes, 64) * 512;
  if (!Number.isSafeInteger(overhead) || overhead < 512)
    throw Error("VMDK 元数据长度无效。");
  return { capacity, size, grain, compressed, flags, overhead };
}
export async function openVmdk(
  file: Pick<ImageFile, "size" | "slice">,
  inflate?: Inflate,
  external?: { size: number; budget: VmdkBudget; parent?: ReadParent },
  stream: ImageReadMode = false,
  chain?: {
    budget: VmdkBudget;
    bind: (
      desc: ReturnType<typeof parseVmdkDescriptor>,
    ) => Promise<ReadParent | undefined>;
  },
): Promise<LogicalImage> {
  const budget = external?.budget ?? chain?.budget ?? vmdkBudget(stream);
  const { physical, capture, verify } = vmdkFileReads(file, budget);
  let logicalBytes = 0,
    expandedBytes = 0;
  const spans: Span[] = [];
  const meta = async (at: number, length: number) => {
    const bytes = await capture(at, length);
    spans.push({ at, end: at + length });
    return bytes;
  };
  const first = await meta(0, 512);
  let active = first;
  const initial = header(first, Boolean(external)),
    footerIndex = dv(first).getBigUint64(56, true) === 0xffffffffffffffffn;
  const notes = [
      "VMDK 索引不含内容校验和；结构检查不能证明整个磁盘的数据完好。",
    ],
    warnings: string[] = [];
  if (footerIndex) {
    if (!initial.compressed || initial.flags & 2)
      throw Error("VMDK 末尾索引布局暂不支持冗余目录或未压缩数据。");
    const footer = await meta(file.size - 1536, 1536);
    if (
      u64(footer, 0) !== 1 ||
      u32(footer, 8) !== 0 ||
      u32(footer, 12) !== 3 ||
      footer.subarray(1024, 1040).some(Boolean)
    )
      throw Error("VMDK 页脚或流结束标记无效。");
    active = footer.subarray(512, 1024);
    header(active);
    if (
      !same(first.subarray(0, 48), active.subarray(0, 48)) ||
      !same(first.subarray(64, 79), active.subarray(64, 79))
    )
      throw Error("VMDK 首尾头部的容量或格式字段冲突。");
  } else if (initial.compressed)
    notes.push(
      "此压缩 VMDK 使用前置索引；按索引读取，没有以流结束标记证明文件完整。",
    );
  const { size, capacity, grain, compressed, flags, overhead } = initial;
  if (overhead > file.size) throw Error("VMDK 元数据区不完整。");
  const descAt = u64(active, 28) * 512,
    descLength = u64(active, 36) * 512;
  let parent = external?.parent;
  if (external) {
    if (size !== external.size || compressed || footerIndex)
      throw Error("VMDK 稀疏分卷的容量、描述符或编码与主描述文件不一致。");
    // QEMU reserves an empty descriptor area even in split extents.
    if (descAt || descLength) {
      if (
        descAt < 512 ||
        descLength < 512 ||
        descLength > 65536 ||
        descAt > overhead - descLength
      )
        throw Error("VMDK 稀疏分卷的空描述符范围无效。");
      if ((await meta(descAt, descLength)).some(Boolean))
        throw Error("VMDK 分卷含另一份描述符，与主描述文件不一致。");
    }
  } else {
    if (
      descAt < 512 ||
      descLength < 512 ||
      descLength > 65536 ||
      descAt > overhead - descLength
    )
      throw Error("VMDK 缺少有界嵌入描述符；若为分卷，请先选择主描述文件。");
    const descriptorBytes = await meta(descAt, descLength);
    if (!descriptorBytes.some(Boolean))
      throw Error("这是 VMDK 数据分卷，请先选择主描述文件。");
    const desc = parseVmdkDescriptor(descriptorBytes);
    if (
      desc.size !== capacity * 512 ||
      desc.extents.length !== 1 ||
      desc.type !== (compressed ? "streamoptimized" : "monolithicsparse")
    )
      throw Error("VMDK 描述符与头部容量或类型不一致。");
    if (desc.parent && !chain)
      throw Error("VMDK 差分层须与显式选择的父盘链一起打开。");
    if (chain) parent = await chain.bind(desc);
  }
  const grains = Math.ceil(size / grain),
    directories = Math.ceil(grains / 512),
    gdLength = align(directories * 4);
  chargeVmdkTables(budget, directories * 2048);
  const claimTable = async (at: number, length: number, type: number) => {
    if (
      at < 512 ||
      (!footerIndex && at + length > overhead) ||
      (footerIndex && at < overhead)
    )
      throw Error("VMDK 目录或粒度表位置无效。");
    if (footerIndex) {
      const marker = await meta(at - 512, 512);
      if (
        u64(marker, 0) !== length / 512 ||
        u32(marker, 8) !== 0 ||
        u32(marker, 12) !== type
      )
        throw Error("VMDK 元数据标记与表长度不一致。");
    }
    return meta(at, length);
  };
  const gdAt = u64(active, 56) * 512,
    rgdAt = flags & 2 ? u64(active, 48) * 512 : 0;
  const gd = await claimTable(gdAt, gdLength, 2),
    rgd = rgdAt ? await claimTable(rgdAt, gdLength, 2) : null;
  if (flags & 2 && !rgdAt) throw Error("VMDK 缺少声明的冗余目录。");
  const mapping = new Uint32Array(grains);
  for (let i = 0; i < directories; i++) {
    const at = u32(gd, i * 4) * 512,
      backup = rgd ? u32(rgd, i * 4) * 512 : 0;
    if (rgd && Boolean(at) !== Boolean(backup))
      throw Error("VMDK 两份目录的分配状态冲突。");
    if (!at) continue;
    const table = await claimTable(at, 2048, 1),
      redundant = backup ? await claimTable(backup, 2048, 1) : null;
    if (redundant && !same(table, redundant))
      throw Error("VMDK 两份粒度表冲突，未自动选择或修复副本。");
    for (let j = 0; j < 512; j++) {
      const entry = u32(table, j * 4),
        index = i * 512 + j;
      if (index >= grains) {
        if (entry !== 0) throw Error("VMDK 容量之外有已分配粒度条目。");
        continue;
      }
      if (entry === 1 && !(flags & 4))
        throw Error("VMDK 未启用零块标记，但映射含保留值 1。");
      mapping[index] = entry;
    }
  }
  disjoint(spans);
  const addresses = new Uint32Array(grains);
  let count = 0;
  for (const entry of mapping) {
    if (entry <= 1) continue;
    const at = entry * 512,
      length = compressed ? 12 : grain;
    vmdkRange(at, length, file.size);
    if (
      at < overhead ||
      (!compressed && at % grain !== 0) ||
      overlaps(spans, at, at + length)
    )
      throw Error("VMDK 数据块与元数据重叠、未对齐或位置无效。");
    addresses[count++] = entry;
  }
  const sorted = addresses.subarray(0, count).sort();
  for (let i = 1; i < count; i++)
    if ((sorted[i] - sorted[i - 1]) * 512 < (compressed ? 12 : grain))
      throw Error("VMDK 数据块重复或重叠。");
  const cache = new Map<number, Uint8Array>();
  const expand = async (index: number, at: number) => {
    const cached = cache.get(index);
    if (cached) return cached;
    if (!inflate) throw Error("压缩 VMDK 需要桌面程序的有界解压支持。");
    expandedBytes += grain;
    if (
      expandedBytes >
      (budget.stream === "browse"
        ? size * 32 + 128 * MiB
        : budget.stream
          ? size * 2 + 64 * MiB
          : 64 * MiB)
    )
      throw Error("VMDK 解压工作量超过 64 MiB 上限。");
    // Data markers are re-parsed in the mandatory second full source pass.
    const marker = await (budget.stream ? physical(at, 12) : capture(at, 12)),
      length = u32(marker, 8);
    if (
      u64(marker, 0) !== (index * grain) / 512 ||
      length < 1 ||
      length > 2 * grain
    )
      throw Error("VMDK 压缩粒度的 LBA 或长度无效。");
    vmdkRange(at, align(12 + length), file.size);
    let lo = 0,
      hi = count;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (sorted[mid] * 512 <= at) lo = mid + 1;
      else hi = mid;
    }
    const end = at + align(12 + length);
    if ((lo < count && end > sorted[lo] * 512) || overlaps(spans, at, end))
      throw Error("VMDK 压缩数据与其他数据块或元数据重叠。");
    const output = await inflate(await physical(at + 12, length), grain);
    if (output.length !== grain) throw Error("VMDK 解压后长度与粒度不一致。");
    cache.set(index, output);
    while (cache.size * grain > 8 * MiB)
      cache.delete(cache.keys().next().value!);
    return output;
  };
  if (compressed)
    notes.push("仅解压并校验分区表读取涉及的数据块；未验证其他压缩块。");
  return {
    size,
    sectorSize: 512,
    format: external
      ? "VMDK · 稀疏分卷"
      : compressed
        ? "VMDK · 压缩流"
        : "VMDK · 单文件稀疏",
    warnings,
    notes,
    async read(at, length) {
      vmdkRange(at, length, size);
      logicalBytes += length;
      if (length > MiB || logicalBytes > logicalLimit(budget.stream, size))
        throw Error("仅允许读取 VMDK 分区表所需的有限字节。");
      const out = new Uint8Array(length);
      for (let done = 0; done < length;) {
        const offset = at + done,
          index = Math.floor(offset / grain),
          within = offset % grain,
          entry = mapping[index];
        let run = Math.min(length - done, grain - within);
        if (entry > 1) {
          const bytes = compressed
            ? (await expand(index, entry * 512)).subarray(within, within + run)
            : await physical(entry * 512 + within, run);
          out.set(bytes, done);
        } else if (entry === 0 && parent) {
          for (
            let next = index + 1;
            done + run < length && mapping[next] === 0;
            next++
          )
            run = Math.min(length - done, run + grain);
          out.set(await parent(offset, run), done);
        }
        done += run;
      }
      return out;
    },
    verify,
  };
}
