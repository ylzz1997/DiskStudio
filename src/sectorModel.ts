export interface SectorSelection {
  target: string;
  layoutKey: string;
  partition: number | null;
}
export interface SectorPage {
  id: string;
  source: SectorSelection;
  label: string;
  sourceOffset: number;
  sourceSize: number;
  sectorSize: number;
  sector: number;
  bytes: number[];
  sha256: string;
}
export interface SectorSearch {
  id: string;
  source: SectorSelection;
  label: string;
  options: { startSector: number; sectorCount: number };
  sectorSize: number;
  query: SectorQuery;
  direction: "forward" | "backward";
  result: {
    state: string;
    checkedBytes: number;
    totalBytes: number;
    hits: number[];
    patternBytes: number;
    message: string;
  };
}
export interface SectorConditionInput {
  encoding: "hex" | "utf8" | "utf16Le";
  value: string;
  offset: string;
  ignoreAsciiCase: boolean;
  textWildcard?: string;
  sectorByte?: string;
}
export interface SectorQuery {
  mode: "any" | "all";
  conditions: {
    pattern: { encoding: SectorConditionInput["encoding"]; value: string };
    offset: number;
    ignoreAsciiCase: boolean;
    textWildcard?: string;
    sectorByte?: number;
  }[];
}
export function buildSectorQuery(
  inputs: SectorConditionInput[],
  mode: SectorQuery["mode"],
  sectorSize = 512,
): SectorQuery {
  if (!inputs.length || inputs.length > 16)
    throw Error("搜索条件须为 1–16 项。");
  return {
    mode,
    conditions: inputs.map((input, i) => {
      const number = i + 1;
      if (!input.value)
        throw new MessageError("请填写条件 {number}。", { number });
      if (!/^(?:\d+|0x[\da-f]+)$/i.test(input.offset.trim()))
        throw new MessageError(
          "条件 {number} 的偏移须为十进制或 0x 开头的十六进制。",
          { number },
        );
      const offset = Number(input.offset.trim());
      if (!Number.isSafeInteger(offset) || offset < 0 || offset >= 65536)
        throw new MessageError("条件 {number} 的偏移须小于 64 KiB。", {
          number,
        });
      const wildcard =
        input.encoding !== "hex" ? input.textWildcard : undefined;
      if (wildcard && (!/^[!-~]$/.test(wildcard) || wildcard === "\\"))
        throw Error("文本通配符须为一个可见 ASCII 字符，反斜杠除外。");
      const sectorByteText = input.sectorByte?.trim() ?? "";
      const sectorByte = sectorByteText ? Number(sectorByteText) : undefined;
      if (
        sectorByte !== undefined &&
        (!/^(?:\d+|0x[\da-f]+)$/i.test(sectorByteText) ||
          !Number.isSafeInteger(sectorByte) ||
          ![512, 4096].includes(sectorSize) ||
          sectorByte < 0 ||
          sectorByte >= sectorSize)
      )
        throw Error("条件的扇区内偏移超出逻辑扇区。");
      return {
        pattern: { encoding: input.encoding, value: input.value },
        offset,
        ignoreAsciiCase: input.encoding !== "hex" && input.ignoreAsciiCase,
        ...(wildcard ? { textWildcard: wildcard } : {}),
        ...(sectorByte !== undefined ? { sectorByte } : {}),
      };
    }),
  };
}
export type SectorUnit = "sector" | "byte" | "word" | "dword" | "qword";
export type SectorOrigin = "start" | "ahead" | "behind" | "end";

export function sectorUnitBytes(unit: SectorUnit, sectorSize: number) {
  return { sector: sectorSize, byte: 1, word: 2, dword: 4, qword: 8 }[unit];
}

export function sectorLocation(
  text: string,
  unit: SectorUnit,
  size: number,
  sectorSize: number,
  origin: SectorOrigin,
  current?: number,
) {
  if (
    !Number.isSafeInteger(size) ||
    ![512, 4096].includes(sectorSize) ||
    size <= 0 ||
    size % sectorSize !== 0
  )
    throw Error("容量或扇区大小无效。");
  if (!/^(?:\d+|0x[\da-f]+)$/i.test(text.trim()))
    throw Error("请输入十进制或 0x 开头的十六进制位置。");
  const value = Number(text.trim());
  const stride = sectorUnitBytes(unit, sectorSize);
  const offset = value * stride;
  if (!Number.isSafeInteger(offset) || offset < 0 || offset >= size)
    throw Error("位置超出所选范围。");
  let target = offset;
  if (origin === "end") target = size - stride - offset;
  if (origin === "ahead" || origin === "behind") {
    if (
      current === undefined ||
      !Number.isSafeInteger(current) ||
      current < 0 ||
      current >= size
    )
      throw Error("请先读取扇区并定位当前字节。");
    target = origin === "ahead" ? current + offset : current - offset;
  }
  if (!Number.isSafeInteger(target) || target < 0 || target >= size)
    throw Error("位置超出所选范围。");
  return { sector: Math.floor(target / sectorSize), byte: target % sectorSize };
}
export function interpretBytes(
  bytes: number[],
  offset: number,
  littleEndian: boolean,
) {
  if (!Number.isInteger(offset) || offset < 0 || offset >= bytes.length)
    return [];
  return [1, 2, 4, 8]
    .filter((n) => offset + n <= bytes.length)
    .map((n) => {
      const selected = bytes.slice(offset, offset + n);
      if (littleEndian) selected.reverse();
      let value = 0n;
      for (const b of selected) value = (value << 8n) | BigInt(b);
      const bits = BigInt(n * 8),
        signed = value & (1n << (bits - 1n)) ? value - (1n << bits) : value;
      return { bits: n * 8, unsigned: String(value), signed: String(signed) };
    });
}
export function differingBytes(a: number[], b: number[]) {
  return Array.from({ length: Math.max(a.length, b.length) }, (_, i) =>
    a[i] !== b[i] ? i : -1,
  ).filter((i) => i >= 0);
}
export interface SectorWritePlan {
  token: string;
  target: string;
  relativeSector: number;
  absoluteSector: number;
  sectorSize: number;
  writeSize: number;
  writeStartLba: number;
  beforeSha256: string;
  afterSha256: string;
  changes: { offset: number; before: number; after: number }[];
  warning: string;
}
export function stageSectorBytes(
  bytes: number[],
  offset: number,
  text: string,
  encoding: SectorConditionInput["encoding"] = "hex",
) {
  let replacement: number[] | Uint8Array;
  if (encoding === "hex") {
    const hex = text.replace(/[\t\n\r ]/g, "");
    if (!/^(?:[\da-f]{2})+$/i.test(hex) || hex.length > 8192)
      throw Error("请输入完整的十六进制字节，例如 01 FF 20。");
    replacement = Array.from({ length: hex.length / 2 }, (_, i) =>
      parseInt(hex.slice(i * 2, i * 2 + 2), 16),
    );
  } else {
    if (!text || /[\uD800-\uDFFF]/u.test(text))
      throw Error("请输入完整的 Unicode 文本。");
    if (text.length > bytes.length) throw Error("改动超出当前扇区。");
    if (encoding === "utf8") {
      replacement = new TextEncoder().encode(text);
    } else {
      replacement = new Uint8Array(text.length * 2);
      const view = new DataView(replacement.buffer);
      for (let i = 0; i < text.length; i++)
        view.setUint16(i * 2, text.charCodeAt(i), true);
    }
  }
  if (
    !Number.isInteger(offset) ||
    offset < 0 ||
    offset + replacement.length > bytes.length
  )
    throw Error("改动超出当前扇区。");
  const next = bytes.slice();
  for (let i = 0; i < replacement.length; i++)
    next[offset + i] = replacement[i];
  return next;
}
import { MessageError } from "./i18n/translate";
