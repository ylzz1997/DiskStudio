// Convert only ordinary synthetic project files. No VM/device interaction.
import { mkdir, readFile, writeFile, lstat } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { gzipSync, gunzipSync } from "node:zlib";
import { fileURLToPath } from "node:url";
const root = new URL("../", import.meta.url),
  output = new URL(".vm/vdi-fixtures/", root),
  fixtures = new URL("tests/fixtures/", root);
await mkdir(output, { recursive: true });
for (const dir of [new URL(".vm/", root), output, fixtures])
  if ((await lstat(dir)).isSymbolicLink())
    throw Error("Fixture directories must not be links");
const tool = fileURLToPath(new URL(".vm/tools/qemu-10.2/qemu-img.exe", root));
console.log(
  execFileSync(tool, ["--version"], {
    encoding: "utf8",
    windowsHide: true,
  }).split("\n")[0],
);
const raw = gunzipSync(await readFile(new URL("vhd-qemu-raw.gz", fixtures)));
const source = new URL("synthetic-extended.raw", output);
await writeFile(source, raw, { flag: "wx" });
for (const kind of ["fixed", "dynamic"]) {
  const dest = new URL(`synthetic-${kind}.vdi`, output);
  try {
    await lstat(dest);
    throw Error("Fixture already exists");
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
  }
  execFileSync(
    tool,
    [
      "convert",
      "-f",
      "raw",
      "-O",
      "vdi",
      "-o",
      `static=${kind === "fixed" ? "on" : "off"}`,
      fileURLToPath(source),
      fileURLToPath(dest),
    ],
    { windowsHide: true },
  );
  const bytes = await readFile(dest);
  execFileSync(tool, ["check", "-f", "vdi", fileURLToPath(dest)], {
    windowsHide: true,
  });
  execFileSync(
    tool,
    [
      "compare",
      "-f",
      "raw",
      "-F",
      "vdi",
      fileURLToPath(source),
      fileURLToPath(dest),
    ],
    { windowsHide: true },
  );
  await writeFile(new URL(`vdi-qemu-${kind}.gz`, fixtures), gzipSync(bytes), {
    flag: "wx",
  });
  console.log(
    `${kind}: ${bytes.length} bytes; QEMU check/compare passed; header ${bytes.readUInt32LE(72)}, allocated ${bytes.readUInt32LE(388)}`,
  );
}
