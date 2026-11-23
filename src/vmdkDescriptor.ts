// Descriptor names are keys into explicitly selected Files, never paths to open.
export interface VmdkExtent {
  name: string;
  kind: "SPARSE" | "FLAT" | "VMFS" | "VMFSSPARSE" | "SESPARSE";
  size: number;
  offset: number;
}
const extentKinds = new Map<string, VmdkExtent["kind"]>([
  ["monolithicflat", "FLAT"],
  ["twogbmaxextentflat", "FLAT"],
  ["monolithicsparse", "SPARSE"],
  ["twogbmaxextentsparse", "SPARSE"],
  ["streamoptimized", "SPARSE"],
  ["vmfs", "VMFS"],
  ["vmfssparse", "VMFSSPARSE"],
  ["sesparse", "SESPARSE"],
]);
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
      /^(?:RW|RDONLY)\s+([0-9]{1,20})\s+(SPARSE|FLAT|VMFS|VMFSSPARSE|SESPARSE)\s+"([^"\r\n]+)"(?:\s+([0-9]{1,20}))?\s*(?:#.*)?$/i.exec(
        line,
      );
    if (extent) {
      const kind = extent[2].toUpperCase() as VmdkExtent["kind"];
      if ((kind === "FLAT") !== (extent[4] !== undefined))
        throw Error("只有 VMDK FLAT 使用描述符中的外部扇区偏移。");
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
    if (
      key.includes("parent") &&
      !["parentcid", "parentfilenamehint"].includes(key)
    )
      throw Error("VMDK 描述符包含不支持的父盘字段。");
    if (
      ![
        "version",
        "cid",
        "parentcid",
        "parentfilenamehint",
        "createtype",
        "encoding",
        "changetrackpath",
      ].includes(key) &&
      !key.startsWith("ddb.")
    )
      throw Error("VMDK 描述符包含未知功能字段。");
    if (/encrypt|keyid|crypt/.test(key)) throw Error("加密 VMDK 暂不支持。");
    values.set(key, value);
  }
  const type = values.get("createtype")?.toLowerCase() ?? "";
  const extentKind = extentKinds.get(type);
  if (!extentKind) throw Error("VMDK 描述符的镜像类型暂不支持。");
  const version = values.get("version");
  const cid = values.get("cid")?.toLowerCase() ?? "",
    parentCid = values.get("parentcid")?.toLowerCase() ?? "";
  if (
    (version !== "1" &&
      !(
        version === "3" && ["vmfs", "vmfssparse", "sesparse"].includes(type)
      )) ||
    !/^[0-9a-f]{8}$/.test(cid) ||
    !/^[0-9a-f]{8}$/.test(parentCid)
  )
    throw Error("VMDK 描述符版本或内容标识无效。");
  const hint = values.get("parentfilenamehint");
  let parent: { cid: string; name: string } | undefined;
  if (parentCid !== "ffffffff") {
    if (
      ![
        "monolithicsparse",
        "twogbmaxextentsparse",
        "vmfssparse",
        "sesparse",
      ].includes(type)
    )
      throw Error("此 VMDK 类型不能作为差分层读取。");
    // Resolve only a basename within the explicit selection. Locator paths
    // from another directory or machine never reach a filesystem API.
    const name = hint?.split(/[\\/]/).at(-1) ?? "";
    if (!/\.vmdk$/i.test(name)) throw Error("VMDK 缺少有效的父盘文件名。");
    vmdkNameKey(name);
    parent = { cid: parentCid, name };
  } else if (hint !== undefined) {
    throw Error("VMDK 基盘标识与父盘提示冲突。");
  }
  if (
    !extents.length ||
    (!type.startsWith("twogb") && extents.length !== 1) ||
    extents.some((e) => e.kind !== extentKind)
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
  return { type, extents, size, cid, parent };
}
