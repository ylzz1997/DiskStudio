import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { memoryFile } from "./vhd-memory";
export const bundleBytes = (name: string) =>
  Uint8Array.from(
    gunzipSync(
      readFileSync(new URL(`vmdk-bundle-${name}.gz`, import.meta.url)),
    ),
  );
export const bundleFile = (name: string) =>
  memoryFile(bundleBytes(name), name).file;
export const bundleNames = {
  flat: ["flat.vmdk", "flat-flat.vmdk"],
  qemu: ["qemu-split.vmdk", "qemu-split-s001.vmdk"],
  sparse: ["split-sparse.vmdk", "part0-s001.vmdk", "part1-s001.vmdk"],
  split: ["split-flat.vmdk", "flat0.bin", "flat1.bin"],
};
