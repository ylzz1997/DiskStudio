import { readFileSync } from "node:fs";
import { gunzipSync, inflateSync } from "node:zlib";
export const fixture = (kind = "sparse") =>
  Uint8Array.from(
    gunzipSync(
      readFileSync(
        new URL(
          `vmdk-${kind === "footer" ? "footer" : "qemu-" + kind}.gz`,
          import.meta.url,
        ),
      ),
    ),
  );
export const raw = () =>
  gunzipSync(readFileSync(new URL("vmdk-raw.gz", import.meta.url)));
export const view = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
export const u32 = (b: Uint8Array, at: number) => view(b).getUint32(at, true);
export const u64 = (b: Uint8Array, at: number) =>
  Number(view(b).getBigUint64(at, true));
export const set32 = (b: Uint8Array, at: number, n: number) =>
  view(b).setUint32(at, n, true);
export const set64 = (b: Uint8Array, at: number, n: number | bigint) =>
  view(b).setBigUint64(at, BigInt(n), true);
export function table(b: Uint8Array, redundant = false) {
  const head =
    view(b).getBigUint64(56, true) === 0xffffffffffffffffn
      ? b.length - 1024
      : 0;
  return u32(b, u64(b, head + (redundant ? 48 : 56)) * 512) * 512;
}
export function entry(b: Uint8Array, index: number, value: number) {
  set32(b, table(b) + index * 4, value);
  if (u32(b, 8) & 2) set32(b, table(b, true) + index * 4, value);
}
export function editDescriptor(b: Uint8Array, edit: (text: string) => string) {
  const at = u64(b, 28) * 512,
    size = u64(b, 36) * 512;
  const text = new TextDecoder()
    .decode(b.subarray(at, at + size))
    .split("\0")[0];
  const replacement = new TextEncoder().encode(edit(text));
  if (replacement.length >= size) throw Error("Descriptor fixture overflow");
  b.fill(0, at, at + size);
  b.set(replacement, at);
}
// Independent oracle, not the product decompressor.
export async function inflate(bytes: Uint8Array, expected: number) {
  return Uint8Array.from(inflateSync(bytes, { maxOutputLength: expected }));
}
