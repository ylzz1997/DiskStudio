// Independent read-only qemu-img comparisons of new synthetic ordinary files.
import { realpathSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { resolve, relative, isAbsolute, sep } from "node:path";
import { spawnSync } from "node:child_process";
const root = realpathSync(resolve(".vm/synthetic-image-creation"));
const corpus = realpathSync(
  process.env.DISKSTUDIO_CREATION_CORPUS ?? "missing-explicit-synthetic-corpus",
);
const rel = relative(root, corpus);
if (!rel || rel.startsWith("..") || isAbsolute(rel) || rel.includes(sep))
  throw Error(
    "Expected one generated corpus below .vm/synthetic-image-creation",
  );
function ordinary(name) {
  const p = realpathSync(resolve(corpus, name));
  if (relative(corpus, p) !== name || !statSync(p).isFile())
    throw Error("Expected ordinary corpus file");
  return p;
}
const expected = ordinary("gpt512.img");
const tool = resolve(".vm/tools/qemu/qemu-img.exe");
const results = [];
for (const [name, driver] of [
  ["fixed.vhd", "vpc"],
  ["dynamic.vhd", "vpc"],
  ["fixed.vdi", "vdi"],
  ["dynamic.vdi", "vdi"],
  ["fixed.vhdx", "vhdx"],
  ["dynamic.vhdx", "vhdx"],
  ["sparse.vmdk", "vmdk"],
  ["structured.hds", "parallels"],
]) {
  const path = ordinary(name);
  // Driver specified explicitly; no probes of disks, backing chains or mounts.
  const q = spawnSync(
    tool,
    ["compare", "-f", "raw", "-F", driver, expected, path],
    { encoding: "utf8", windowsHide: true, timeout: 60000 },
  );
  const output = (q.stdout ?? "") + (q.stderr ?? "");
  results.push({
    name,
    driver,
    exitCode: q.status,
    error: q.error?.message,
    output: output.trim(),
  });
}
writeFileSync(
  resolve(".vm/image-creation-qemu.json"),
  JSON.stringify({ corpus, results }, null, 2),
);
console.log(JSON.stringify(results, null, 2));
if (
  results.some(
    (r) => r.exitCode !== 0 || !r.output.includes("Images are identical"),
  )
)
  process.exitCode = 1;
