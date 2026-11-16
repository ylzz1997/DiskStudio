import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { expect, it } from "vitest";
import cases from "../tests/recovery-query-cases.json";
const source = readFileSync(
  new URL("../src-tauri/src/directory_query.js", import.meta.url),
  "utf8",
);
const queryEngine = runInNewContext(
  `${source}\ndirectoryQuery`,
  { TextEncoder },
  { timeout: 1000 },
);
const files = cases.files.map((f) => ({
  ...f,
  bytes: f.size,
  metadata: { modified: f.modified },
}));
it("shares exact query, date, precision and ordering semantics with native results", () => {
  for (const c of cases.cases) {
    const run = () => {
      const q = queryEngine.prepare(c.query, c.filter);
      return q
        .ordered(files.filter(q.matches))
        .map((f: { id: number }) => f.id);
    };
    if ("error" in c) expect(run, c.label).toThrow();
    else expect(run(), c.label).toEqual(c.expected);
  }
});
it("orders the entire result before paging without losing identities", () => {
  const files = Array.from({ length: 305 }, (_, id) => ({
    id,
    name: `file-${id}`,
    bytes: String(id),
  }));
  const q = queryEngine.prepare("", { sortBy: "size", sortDirection: "desc" });
  const rows = q.ordered(files);
  expect([rows[0].id, rows[100].id, rows[300].id, rows[304].id]).toEqual([
    304, 204, 4, 0,
  ]);
  expect(new Set(rows.map((f: { id: number }) => f.id)).size).toBe(305);
});
it("rejects invalid dates and Unicode queries exceeding the byte budget", () => {
  expect(() => queryEngine.prepare("中".repeat(67))).toThrow();
  expect(() => queryEngine.prepare("中".repeat(66))).not.toThrow();
  for (const d of ["0001-01-01", "2000-02-29", "9999-12-31"])
    expect(queryEngine.date(d)).toBe(d);
  for (const d of [
    "1900-02-29",
    "2024-13-01",
    "2024-01-00",
    "2024-01-01\n",
    "２０２４-01-01",
  ])
    expect(queryEngine.date(d)).toBe(null);
});
