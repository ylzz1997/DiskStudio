import {
  logicalLimit,
  type ImageFile,
  type ImageReadMode,
  type LogicalImage,
} from "./imageSource";
import { openVhdLayer, vhdBudget, type VhdLayer } from "./vhdImage";
import { MissingImageFilesError } from "./imageDependencies";
const EPOCH = Date.UTC(2000, 0, 1);
const MiB = 1024 ** 2;
const uuid = (b: Uint8Array) =>
  Array.from(b, (n) => n.toString(16).padStart(2, "0")).join("");
export async function openVhdChain(
  file: ImageFile,
  parents: ImageFile[] = [],
  stream: ImageReadMode = false,
): Promise<LogicalImage> {
  if (parents.length > 15)
    throw Error("VHD 差分链最多包含 16 层（含当前镜像）。");
  const budget = vhdBudget(stream),
    leaf = await openVhdLayer(file, budget);
  if (!leaf.parent) return leaf;
  const layers: { file: ImageFile; image: VhdLayer }[] = [
    { file, image: leaf },
  ];
  const candidates = new Map<string, ImageFile>();
  // Probe only explicitly selected files' footers; do not follow stored locators.
  for (const parent of parents) {
    if (
      !/\.(a?vhd)$/i.test(parent.name) ||
      /[\\/:\0]/.test(parent.name) ||
      parent.name.length > 255
    )
      throw Error("父盘必须是所选的普通 VHD/AVHD 文件。");
    if (
      !Number.isSafeInteger(parent.size) ||
      parent.size < 2048 ||
      parent.size > 2 ** 41 ||
      parent.size % 512
    )
      throw Error("所选 VHD 父盘文件长度无效。");
    const b = new Uint8Array(
      await parent.slice(parent.size - 512).arrayBuffer(),
    );
    budget.bytes += 512;
    budget.calls++;
    if (
      b.length !== 512 ||
      new TextDecoder().decode(b.subarray(0, 8)) !== "conectix"
    )
      throw Error("所选父盘缺少完整 VHD 文件尾。");
    let sum = 0;
    for (let i = 0; i < 512; i++) if (i < 64 || i >= 68) sum += b[i];
    if (
      ~sum >>> 0 !==
      new DataView(b.buffer, b.byteOffset, b.byteLength).getUint32(64)
    )
      throw Error("所选 VHD 父盘校验和无效。");
    const id = uuid(b.subarray(68, 84));
    if (/^0+$/.test(id) || candidates.has(id) || id === leaf.id)
      throw Error("所选 VHD 标识为空、重复或形成循环，无法唯一选择父盘。");
    candidates.set(id, parent);
  }
  const seen = new Set([leaf.id]);
  while (layers.at(-1)!.image.parent) {
    const need = layers.at(-1)!.image.parent!;
    if (seen.has(need.id)) throw Error("VHD 父盘链存在循环。");
    if (layers.length >= 16) throw Error("VHD 差分链超过 16 层上限。");
    const parent = candidates.get(need.id);
    if (!parent)
      throw new MissingImageFilesError(
        [`${need.name || "父盘文件"} · 标识 ${need.id}`],
        "parent",
      );
    const seconds = Math.floor((parent.lastModified - EPOCH) / 1000);
    if (
      !Number.isSafeInteger(parent.lastModified) ||
      seconds !== need.timestamp
    )
      throw Error(
        `父盘 ${parent.name} 的修改时间与差分盘记录不一致。请使用创建差分盘时的原始父盘副本，未忽略时间检查。`,
      );
    const image = await openVhdLayer(parent, budget);
    if (image.id !== need.id || image.size !== leaf.size)
      throw Error("VHD 父盘标识或容量不一致。");
    seen.add(image.id);
    layers.push({ file: parent, image });
  }
  let logicalBytes = 0;
  const read = async (
    index: number,
    at: number,
    length: number,
  ): Promise<Uint8Array> => {
    if (index >= layers.length) throw Error("VHD 父盘链不完整。");
    return layers[index].image.read(
      at,
      length,
      index + 1 < layers.length ? (a, n) => read(index + 1, a, n) : undefined,
    );
  };
  return {
    size: leaf.size,
    sectorSize: 512,
    format: "VHD · 差分链",
    fileCount: layers.length,
    fileSize: layers.reduce((n, l) => n + l.file.size, 0),
    chain: layers.map((l) => l.file.name),
    warnings: [],
    notes: [
      `读取顺序：${layers.map((l) => l.file.name).join(" → ")}。`,
      "已逐层核对父盘标识、容量与文件修改时间；这些字段不是内容摘要，不能证明整个链的数据完好。",
      "只读取明确选中的文件，镜像内保存的父盘路径不会被访问。请使用虚拟机关闭后的完整副本。",
      ...(parents.length + 1 > layers.length
        ? ["其他所选文件仅检查文件尾，未读取其分区或内容。"]
        : []),
    ],
    async read(at, length) {
      if (
        !Number.isSafeInteger(at) ||
        !Number.isSafeInteger(length) ||
        at < 0 ||
        length < 1 ||
        at > leaf.size - length
      )
        throw Error("VHD 读取范围越界。");
      logicalBytes += length;
      if (length > MiB || logicalBytes > logicalLimit(stream, leaf.size))
        throw Error("仅允许读取 VHD 分区表所需的有限字节。");
      return read(0, at, length);
    },
    async verify() {
      for (const layer of layers) await layer.image.verify();
    },
  };
}
