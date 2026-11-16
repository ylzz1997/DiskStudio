// Independent extraction of an explicitly generated ordinary-file corpus.
// 7-Zip 26.06 rejects odd sector counts (VhdxHandler.cpp size-bit check), so
// those original files retain their separate Rust/TS evidence, not a 7z pass.
import { readFileSync, realpathSync, writeFileSync } from "node:fs";
import { resolve, relative, isAbsolute, sep } from "node:path";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
const root = realpathSync(resolve(".vm/synthetic-image-conversion"));
const corpus = realpathSync(process.argv[2]);
const rel = relative(root, corpus);
if (!rel || rel.startsWith("..") || isAbsolute(rel) || rel.includes(sep))
  throw Error("Expected generated synthetic corpus");
const digest = (b) => createHash("sha256").update(b).digest("hex");
const expected = readFileSync(resolve(corpus, "source4k-even.raw"));
const results = [];
for (const name of ["dynamic4k-even.vhdx", "fixed4k-even.vhdx"]) {
  const r = spawnSync(
    resolve(".vm/tools/7zip/7z.exe"),
    ["e", "-so", "-tVHDX", resolve(corpus, name)],
    { windowsHide: true, maxBuffer: 64 * 1024 ** 2 },
  );
  const result = {
    name,
    exitCode: r.status,
    bytes: r.stdout?.length,
    sha256: r.stdout ? digest(r.stdout) : null,
    error: r.error?.message ?? r.stderr?.toString(),
  };
  results.push(result);
  if (r.status !== 0 || !r.stdout.equals(expected))
    throw Error(JSON.stringify(result));
}
writeFileSync(
  resolve(".vm/image-conversion-next-7zip.json"),
  JSON.stringify({ corpus, tool: "7-Zip 26.06", results }, null, 2),
);
console.log(JSON.stringify(results, null, 2));
