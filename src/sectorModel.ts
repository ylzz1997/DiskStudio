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
}
export interface SectorQuery {
  mode: "any" | "all";
  conditions: {
    pattern: { encoding: SectorConditionInput["encoding"]; value: string };
    offset: number;
    ignoreAsciiCase: boolean;
  }[];
}
export function buildSectorQuery(
  inputs: SectorConditionInput[],
  mode: SectorQuery["mode"],
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
      return {
        pattern: { encoding: input.encoding, value: input.value },
        offset,
        ignoreAsciiCase: input.encoding !== "hex" && input.ignoreAsciiCase,
      };
    }),
  };
}
export function sectorPosition(
  text: string,
  unit: "sector" | "byte",
  size: number,
  sectorSize: number,
) {
  if (
    !Number.isSafeInteger(size) ||
    ![512, 4096].includes(sectorSize) ||
    size % sectorSize !== 0
  )
    throw Error("容量或扇区大小无效。");
  if (!/^(?:\d+|0x[\da-f]+)$/i.test(text.trim()))
    throw Error("请输入十进制或 0x 开头的十六进制位置。");
  const value = Number(text.trim());
  if (
    !Number.isSafeInteger(value) ||
    value < 0 ||
    value >= (unit === "byte" ? size : size / sectorSize)
  )
    throw Error("位置超出所选范围。");
  return unit === "byte" ? Math.floor(value / sectorSize) : value;
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
  beforeSha256: string;
  afterSha256: string;
  changes: { offset: number; before: number; after: number }[];
  warning: string;
}
export function stageSectorBytes(
  bytes: number[],
  offset: number,
  text: string,
) {
  const hex = text.replace(/[\t\n\r ]/g, "");
  if (!/^(?:[\da-f]{2})+$/i.test(hex) || hex.length > 8192)
    throw Error("请输入完整的十六进制字节，例如 01 FF 20。");
  if (
    !Number.isInteger(offset) ||
    offset < 0 ||
    offset + hex.length / 2 > bytes.length
  )
    throw Error("改动超出当前扇区。");
  const next = bytes.slice();
  for (let i = 0; i < hex.length; i += 2)
    next[offset + i / 2] = parseInt(hex.slice(i, i + 2), 16);
  return next;
}
import { MessageError } from "./i18n/translate";
