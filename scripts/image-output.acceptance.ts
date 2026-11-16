import { it, expect } from "vitest";
import { readFileSync, realpathSync } from "node:fs";
import { resolve, relative, isAbsolute, sep } from "node:path";
import { createHash } from "node:crypto";
import { openContainer } from "../src/imageReader";

const root = realpathSync(resolve(".vm/synthetic-image-conversion"));
const corpus = realpathSync(
  process.env.DISKSTUDIO_IMAGE_CORPUS ?? "missing-explicit-synthetic-corpus",
);
const rel = relative(root, corpus);
if (!rel || rel.startsWith("..") || isAbsolute(rel) || rel.includes(sep))
  throw Error(
    "Corpus must be one generated directory below .vm/synthetic-image-conversion",
  );
const hash = (b: Uint8Array) => createHash("sha256").update(b).digest("hex");
for (const name of [
  "fixed.vhd",
  "dynamic.vhd",
  "dynamic.vdi",
  "dynamic.vhdx",
  "fixed.vhdx",
  "sparse.vmdk",
  "structured.hds",
  "dynamic4k.vhdx",
  "fixed4k.vhdx",
  "dynamic4k-even.vhdx",
  "fixed4k-even.vhdx",
]) {
  it(`production reader independently decodes complete Rust output ${name}`, async () => {
    const expected = readFileSync(
      resolve(
        corpus,
        name.includes("even")
          ? "source4k-even.raw"
          : name.includes("4k")
            ? "source4k.raw"
            : "source.raw",
      ),
    );
    const bytes = readFileSync(resolve(corpus, name));
    const file = new File([bytes], name);
    const image = await openContainer(file, [], true);
    expect(image).not.toBeNull();
    expect(image!.size).toBe(expected.length);
    expect(image!.sectorSize).toBe(name.includes("4k") ? 4096 : 512);
    await image!.verify();
    const digest = createHash("sha256");
    for (let at = 0; at < image!.size; at += 1024 ** 2) {
      const count = Math.min(1024 ** 2, image!.size - at);
      const b = await image!.read(at, count);
      expect(b.length).toBe(count);
      digest.update(b);
    }
    await image!.verify();
    expect(digest.digest("hex")).toBe(hash(expected));
  });
}
