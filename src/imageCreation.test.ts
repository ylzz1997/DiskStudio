import { expect, it } from "vitest";
import { creationOptions } from "./imageCreation";
it("preserves exact capacities and sector alignment without floating point rounding", () => {
  expect(
    creationOptions("0.5", "GiB", 4096, "vhdxDynamic", "gpt").output.size,
  ).toBe(512 * 1024 ** 2);
  expect(creationOptions("8", "TiB", 4096, "raw", "gpt").output.size).toBe(
    2 ** 43,
  );
  expect(creationOptions("64", "MiB", 512, "vdiFixed", "none").table).toBe(
    "none",
  );
  for (const n of [
    "",
    "0",
    "-1",
    "NaN",
    "Infinity",
    "1e3",
    "9",
    "1.000000001",
  ]) {
    expect(() => creationOptions(n, "TiB", 512, "raw", "none")).toThrow();
  }
  expect(() => creationOptions("4.000001", "MiB", 512, "raw", "gpt")).toThrow();
});
it("checks MBR addressability and container sector and capacity constraints", () => {
  expect(creationOptions("2", "TiB", 512, "raw", "mbr").output.size).toBe(
    2 ** 41,
  );
  expect(() => creationOptions("2.5", "TiB", 512, "raw", "mbr")).toThrow("MBR");
  expect(() => creationOptions("64", "MiB", 4096, "raw", "mbr")).toThrow("MBR");
  expect(() => creationOptions("64", "MiB", 4096, "vdiFixed", "gpt")).toThrow(
    "512",
  );
  expect(() => creationOptions("2", "TiB", 512, "vhdFixed", "gpt")).toThrow(
    "2040",
  );
});
