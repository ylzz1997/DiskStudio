import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { memoryFile, be32, seal } from "./vhd-memory";
export const stamp = Date.UTC(2026, 0, 2, 3, 4, 5);
export const chainBytes = (name: string) =>
  Uint8Array.from(
    gunzipSync(readFileSync(new URL(`vhd-chain-${name}.gz`, import.meta.url))),
  );
export function chainFile(
  name: string,
  bytes = chainBytes(name),
  selectedName = name,
) {
  const result = memoryFile(bytes, selectedName);
  Object.assign(result.file, { lastModified: stamp });
  return result;
}
export function changeFooter(
  b: Uint8Array,
  action: (footer: Uint8Array) => void,
) {
  const f = b.subarray(b.length - 512);
  action(f);
  seal(f, 64);
  if (new DataView(f.buffer, f.byteOffset).getUint32(60) !== 2) b.set(f, 0);
}
export function changeHeader(
  b: Uint8Array,
  action: (header: Uint8Array) => void,
) {
  const h = b.subarray(512, 1536);
  action(h);
  seal(h, 36);
}
export function emptyChild(id: number, parentId: Uint8Array) {
  const original = chainBytes("leaf.avhd"),
    result = new Uint8Array(3584);
  result.set(original.subarray(0, 3072));
  result.set(original.subarray(0, 512), 3072);
  result.fill(255, 1536, 2048);
  changeFooter(result, (f) => {
    f.fill(0, 68, 84);
    f[83] = id;
  });
  changeHeader(result, (h) => {
    h.set(parentId, 40);
    be32(h, 56, (stamp - Date.UTC(2000, 0, 1)) / 1000);
  });
  return result;
}
