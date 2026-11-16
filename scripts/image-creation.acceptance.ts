import { expect, it } from "vitest";
import { readFileSync, realpathSync } from "node:fs";
import { resolve, relative, isAbsolute, sep } from "node:path";
import { createHash } from "node:crypto";
import { openContainer } from "../src/imageReader";
const root = realpathSync(resolve(".vm/synthetic-image-creation"));
const corpus = realpathSync(
  process.env.DISKSTUDIO_CREATION_CORPUS ?? "missing-explicit-synthetic-corpus",
);
const rel = relative(root, corpus);
if (!rel || rel.startsWith("..") || isAbsolute(rel) || rel.includes(sep))
  throw Error("Expected generated image creation corpus under .vm");
const records = JSON.parse(
  readFileSync(resolve(corpus, "manifest.json"), "utf8"),
) as {
  name: string;
  sector: number;
  table: string;
  verified: { logicalBytes: number; sha256: string };
}[];
if (records.length !== 14) throw Error("Unexpected corpus");
for (const r of records) {
  it(`independently decodes generated ${r.name}`, async () => {
    if (!/^[a-z0-9.]+$/.test(r.name)) throw Error("Unexpected file name");
    const bytes = readFileSync(resolve(corpus, r.name));
    const file = new File([bytes], r.name);
    const image = await openContainer(file, [], true);
    const digest = createHash("sha256");
    if (image) {
      expect(image.size).toBe(r.verified.logicalBytes);
      expect(image.sectorSize).toBe(r.sector);
      await image.verify();
      for (let at = 0; at < image.size; at += 1024 ** 2)
        digest.update(
          await image.read(at, Math.min(1024 ** 2, image.size - at)),
        );
      await image.verify();
    } else {
      expect(file.size).toBe(r.verified.logicalBytes);
      digest.update(bytes);
    }
    expect(digest.digest("hex")).toBe(r.verified.sha256);
    if (r.table === "gpt") {
      const raw = readFileSync(
        resolve(corpus, r.sector === 4096 ? "gpt4k.img" : "gpt512.img"),
      );
      expect(createHash("sha256").update(raw).digest("hex")).toBe(
        r.verified.sha256,
      );
      expect(raw.subarray(r.sector, r.sector + 8).toString()).toBe("EFI PART");
      expect(
        raw
          .subarray(raw.length - r.sector, raw.length - r.sector + 8)
          .toString(),
      ).toBe("EFI PART");
    }
  });
}
