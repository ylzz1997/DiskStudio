// Descriptor names are keys into explicitly selected Files, never paths to open.
export interface VmdkExtent {
  name: string;
  kind: "SPARSE" | "FLAT";
  size: number;
  offset: number;
}
export function vmdkNameKey(name: string) {
  if (
    !name ||
    name.length > 255 ||
    name !== name.trim() ||
    /[<>:"/\\|?*\x00-\x1f\x7f]/.test(name) ||
    /[. ]$/.test(name) ||
    /^(?:con|prn|aux|nul|com[1-9¹²³]|lpt[1-9¹²³])(?:\.|$)/i.test(name)
  )
    throw Error("VMDK 外部路径或设备引用暂不支持，未打开引用目标。");
  return name.normalize("NFC").toLowerCase();
}
function sectorBytes(text: string) {
  const value = BigInt(text) * 512n;
  if (value > 2n ** 43n) throw Error("VMDK 范围超出 8 TiB 上限。");
  return Number(value);
}
export function parseVmdkDescriptor(bytes: Uint8Array) {
  if (!bytes.length || bytes.length > 65536)
    throw Error("VMDK 描述符须在 64 KiB 以内。");
  const zero = bytes.indexOf(0);
  if (zero >= 0 && bytes.subarray(zero).some(Boolean))
    throw Error("VMDK 描述符终止符后含不支持的数据。");
  let text: string;
  try {
    text = new TextDecoder("utf-8", { fatal: true }).decode(
      zero < 0 ? bytes : bytes.subarray(0, zero),
    );
  } catch {
    throw Error("VMDK 描述符编码不是有效 UTF-8/ASCII。");
  }
  if (/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/.test(text))
    throw Error("VMDK 描述符含控制字符。");
  const values = new Map<string, string>(),
    extents: VmdkExtent[] = [];
  const lines = text.split(/\r?\n/);
  if (lines.length > 4096) throw Error("VMDK 描述符行数超过上限。");
  for (const raw of lines) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    if (line.length > 4096) throw Error("VMDK 描述符单行过长。");
    const extent =
      /^(?:RW|RDONLY)\s+([0-9]{1,20})\s+(SPARSE|FLAT)\s+"([^"\r\n]+)"(?:\s+([0-9]{1,20}))?\s*(?:#.*)?$/i.exec(
        line,
      );
    if (extent) {
      const kind = extent[2].toUpperCase() as VmdkExtent["kind"];
      if ((kind === "FLAT") !== (extent[4] !== undefined))
        throw Error("VMDK FLAT 需要扇区偏移；SPARSE 不接受外部偏移。");
      vmdkNameKey(extent[3]);
      const size = sectorBytes(extent[1]),
        offset = sectorBytes(extent[4] ?? "0");
      if (!size || extents.length >= 64)
        throw Error("VMDK 范围为空或超过 64 个分卷上限。");
      extents.push({ name: extent[3], kind, size, offset });
      continue;
    }
    const field =
      /^([A-Za-z][A-Za-z0-9_.]*)\s*=\s*(?:"([^"\r\n]*)"|([^\s"#]+))\s*(?:#.*)?$/.exec(
        line,
      );
    if (!field) throw Error("VMDK 描述符含不支持的范围类型或语法。");
    const key = field[1].toLowerCase(),
      value = field[2] ?? field[3];
    if (values.has(key) || values.size >= 128)
      throw Error("VMDK 描述符字段重复或超过上限。");
    if (key.includes("parent") && key !== "parentcid")
      throw Error("暂不支持 VMDK 父盘链，未查找父盘。");
    if (
      !["version", "cid", "parentcid", "createtype", "encoding"].includes(
        key,
      ) &&
      !key.startsWith("ddb.")
    )
      throw Error("VMDK 描述符包含未知功能字段。");
    if (/encrypt|keyid|crypt/.test(key)) throw Error("加密 VMDK 暂不支持。");
    values.set(key, value);
  }
  if (
    values.get("version") !== "1" ||
    !/^[0-9a-f]{8}$/i.test(values.get("cid") ?? "")
  )
    throw Error("VMDK 描述符版本或内容标识无效。");
  if (values.get("parentcid")?.toLowerCase() !== "ffffffff")
    throw Error("VMDK 缺少独立基盘标识或需要父盘，未读取父盘。");
  const type = values.get("createtype")?.toLowerCase() ?? "";
  const flat = ["monolithicflat", "twogbmaxextentflat"].includes(type);
  if (
    !flat &&
    !["monolithicsparse", "streamoptimized", "twogbmaxextentsparse"].includes(
      type,
    )
  )
    throw Error("VMDK 描述符的镜像类型暂不支持。");
  if (
    !extents.length ||
    (!type.startsWith("twogb") && extents.length !== 1) ||
    extents.some((e) => e.kind !== (flat ? "FLAT" : "SPARSE"))
  )
    throw Error("VMDK 描述符的类型与分卷列表不一致。");
  const size = extents.reduce((n, e) => n + e.size, 0);
  if (size < 4 * 1024 ** 2 || size > 2 ** 43)
    throw Error("VMDK 逻辑容量须为 4 MiB 至 8 TiB。");
  const encoding = values.get("encoding")?.toLowerCase();
  if (encoding && !["utf-8", "ascii", "us-ascii"].includes(encoding))
    throw Error("VMDK 描述符编码暂不支持。");
  if (
    values.has("ddb.logicalsectorsize") &&
    values.get("ddb.logicalsectorsize") !== "512"
  )
    throw Error("VMDK 当前仅支持 512 B 逻辑扇区。");
  if (
    values.has("ddb.physicalsectorsize") &&
    !["512", "4096"].includes(values.get("ddb.physicalsectorsize")!)
  )
    throw Error("VMDK 物理扇区字段暂不支持。");
  return { type, extents, size };
}
