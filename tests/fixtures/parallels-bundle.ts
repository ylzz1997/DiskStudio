import { Buffer } from "node:buffer";
import { createHash } from "node:crypto";

export function makeHds(
  size: number,
  cluster: number,
  blocks: [number, Uint8Array][],
  legacy = false,
) {
  const entries = Math.ceil(size / cluster);
  const alignment = legacy ? 512 : cluster;
  const dataStart = Math.ceil((64 + entries * 4) / alignment) * alignment;
  const bytes = Buffer.alloc(dataStart + blocks.length * cluster);
  bytes.write(legacy ? "WithoutFreeSpace" : "WithouFreSpacExt");
  bytes.writeUInt32LE(2, 16);
  bytes.writeUInt32LE(16, 20);
  bytes.writeUInt32LE(Math.floor(size / (512 * 16 * 63)), 24);
  bytes.writeUInt32LE(cluster / 512, 28);
  bytes.writeUInt32LE(entries, 32);
  bytes.writeBigUInt64LE(BigInt(size / 512), 36);
  bytes.writeUInt32LE(0x312e3276, 44);
  bytes.writeUInt32LE(dataStart / 512, 48);
  for (const [index, [at, data]] of blocks.entries()) {
    if (at % cluster || at >= size || data.length !== cluster)
      throw Error("Invalid Parallels fixture block");
    const physical = dataStart + index * cluster;
    bytes.writeUInt32LE(physical / alignment, 64 + (at / cluster) * 4);
    bytes.set(data, physical);
  }
  return bytes;
}

export function withDirtyBitmap(
  source: Uint8Array,
  cluster: number,
  sectors: number,
) {
  const image = Buffer.alloc(source.length + cluster * 2);
  image.set(source);
  const bitmapAt = source.length;
  const headerAt = bitmapAt + cluster;
  const extension = image.subarray(headerAt);
  image.writeBigUInt64LE(BigInt(headerAt / 512), 56);
  image[bitmapAt] = 0x42;
  extension.writeBigUInt64LE(0xab234cef23dcea87n, 0);
  extension.writeBigUInt64LE(0x20385fae252cb34an, 24);
  extension.writeUInt32LE(40, 40);
  extension.writeBigUInt64LE(BigInt(sectors), 48);
  extension.fill(0x62, 56, 72);
  extension.writeUInt32LE(1, 72);
  extension.writeUInt32LE(1, 76);
  extension.writeBigUInt64LE(BigInt(bitmapAt / 512), 80);
  extension.set(createHash("md5").update(extension.subarray(24)).digest(), 8);
  return image;
}
