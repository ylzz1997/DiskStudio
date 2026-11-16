// Cross-language acceptance of a fixed, newly generated ordinary-file corpus.
// This is an explicit acceptance command, never an ambient device scan.
import { expect, it } from "vitest";
import { readFileSync, lstatSync, realpathSync } from "node:fs";
import { resolve, relative } from "node:path";
import { createHash } from "node:crypto";
import { browseReader } from "../src/imageBrowserSource";
const root = resolve(".vm/payload"),
  corpus = resolve(root, "ImageBrowseContainers");
for (const dir of [resolve(".vm"), root, corpus])
  if (
    lstatSync(dir).isSymbolicLink() ||
    realpathSync(dir).toLowerCase() !== dir.toLowerCase()
  )
    throw Error("Expected ordinary generated corpus directory");
function ordinary(dir: string, name: string) {
  if (!/^[a-z0-9.-]+$/.test(name)) throw Error("Invalid fixture name");
  const path = resolve(dir, name);
  if (
    relative(dir, path) !== name ||
    !lstatSync(path).isFile() ||
    lstatSync(path).isSymbolicLink()
  )
    throw Error("Expected ordinary fixture file");
  return readFileSync(path);
}
const records = JSON.parse(ordinary(corpus, "manifest.json").toString()) as {
  name: string;
  input: string;
  sector: 512 | 4096;
  verified: { logicalBytes: number; sha256: string };
}[];
if (records.length !== 10) throw Error("Unexpected fixture corpus");
for (const r of records)
  it(`browses generated ${r.name} using retained-file requests`, async () => {
    const bytes = ordinary(corpus, r.name),
      raw = ordinary(resolve(root, "ImageBrowse"), r.input);
    const group = {
      id: "fixture",
      files: [
        { id: "file", name: r.name, size: bytes.length, lastModified: 0 },
      ],
    };
    const image = await browseReader(
      group,
      { kind: "disk", sector: 512 },
      async (g, f, at, len) => {
        expect([g, f]).toEqual(["fixture", "file"]);
        expect(len).toBeLessThanOrEqual(1024 ** 2);
        return bytes.subarray(at, at + len);
      },
    );
    expect(image.size).toBe(r.verified.logicalBytes);
    expect(image.sectorSize).toBe(r.sector);
    const digest = createHash("sha256");
    for (let at = 0; at < image.size; at += 65536) {
      const data = await image.read(at, Math.min(65536, image.size - at));
      digest.update(data);
      expect(Buffer.from(data)).toEqual(raw.subarray(at, at + data.length));
    }
    expect(digest.digest("hex")).toBe(r.verified.sha256);
    expect(Buffer.from(await image.read(r.sector, 8)).toString()).toBe(
      "EFI PART",
    );
    expect(Buffer.from(await image.read(1024 ** 2 + 3, 8)).toString()).toBe(
      "NTFS    ",
    );
    await image.verify();
  });
