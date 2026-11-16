import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
export const fixture = (kind = "extended") =>
  Uint8Array.from(
    gunzipSync(readFileSync(new URL(`parallels-${kind}.gz`, import.meta.url))),
  );
export const raw = () =>
  gunzipSync(readFileSync(new URL("vmdk-raw.gz", import.meta.url)));
export const view = (b: Uint8Array) =>
  new DataView(b.buffer, b.byteOffset, b.byteLength);
export const get32 = (b: Uint8Array, at: number) => view(b).getUint32(at, true);
export const set32 = (b: Uint8Array, at: number, n: number) =>
  view(b).setUint32(at, n, true);
export const set64 = (b: Uint8Array, at: number, n: number | bigint) =>
  view(b).setBigUint64(at, BigInt(n), true);
