import { imageOutputOptions, type ImageOutputFormat } from "./imageConversion";
export type ImageTable = "none" | "gpt" | "mbr";
export type CapacityUnit = "MiB" | "GiB" | "TiB";
export const tableLabels = { none: "空白（无分区表）", gpt: "GPT", mbr: "MBR" };
export function creationOptions(
  capacity: string,
  unit: CapacityUnit,
  sector: number,
  format: ImageOutputFormat,
  table: ImageTable,
) {
  // Decimal input converted exactly: never silently round disk capacity.
  if (!/^\d{1,8}(?:\.\d{1,9})?$/.test(capacity))
    throw Error("请输入有效的正数容量，最多九位小数。");
  const [whole, fraction = ""] = capacity.split(".");
  const denominator = 10n ** BigInt(fraction.length);
  const amount = BigInt(whole + fraction);
  const scale = { MiB: 20n, GiB: 30n, TiB: 40n }[unit];
  if (scale === undefined || !Object.hasOwn(tableLabels, table))
    throw Error("镜像选项无效。");
  const numerator = amount * (1n << scale);
  if (numerator % denominator) throw Error("容量无法精确表示为整数个字节。");
  const output = imageOutputOptions(
    Number(numerator / denominator),
    sector,
    format,
  );
  if (table === "mbr" && (sector !== 512 || output.size > 2 ** 41))
    throw Error("MBR 镜像创建限 512 B 扇区、最大 2 TiB；其他容量请选择 GPT。");
  return { output, table };
}
