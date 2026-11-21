import { describe, it, expect } from "vitest";
import {
  sectorLocation,
  interpretBytes,
  differingBytes,
  stageSectorBytes,
  buildSectorQuery,
} from "./sectorModel";
describe("sector positions and comparison", () => {
  it("builds typed criteria with explicit bases and preserves text whitespace", () => {
    const query = buildSectorQuery(
      [
        {
          encoding: "hex",
          value: "46 41 54 ?? ??",
          offset: "0x52",
          ignoreAsciiCase: true,
        },
        {
          encoding: "utf8",
          value: " a ",
          offset: "510",
          ignoreAsciiCase: true,
        },
      ],
      "all",
    );
    expect(query.conditions.map((c) => c.offset)).toEqual([82, 510]);
    expect(query.conditions[0].ignoreAsciiCase).toBe(false);
    expect(query.conditions[1]).toEqual({
      pattern: { encoding: "utf8", value: " a " },
      offset: 510,
      ignoreAsciiCase: true,
    });
    for (const offset of [
      "-1",
      "1.5",
      "1e3",
      "65536",
      "0x10000",
      "",
      "Infinity",
    ])
      expect(() =>
        buildSectorQuery(
          [{ encoding: "hex", value: "FF", offset, ignoreAsciiCase: false }],
          "any",
        ),
      ).toThrow();
    expect(() => buildSectorQuery([], "all")).toThrow();
    expect(() =>
      buildSectorQuery(
        Array(17).fill({
          encoding: "hex",
          value: "FF",
          offset: "0",
          ignoreAsciiCase: false,
        }),
        "any",
      ),
    ).toThrow();
  });
  it("stages bounded byte ranges without changing the read snapshot", () => {
    const original = Array(512).fill(0),
      staged = stageSectorBytes(original, 510, "01 FF");
    expect(staged.slice(510)).toEqual([1, 255]);
    expect(original[510]).toBe(0);
    expect(stageSectorBytes(staged, 510, "00 00")).toEqual(original);
    for (const text of ["F", "GG", "", "0xAA"])
      expect(() => stageSectorBytes(original, 0, text)).toThrow();
    expect(() => stageSectorBytes(original, 511, "01 FF")).toThrow();
    expect(() => stageSectorBytes(original, -1, "00")).toThrow();
  });
  it("adds opt-in wildcard and sector placement to the typed query", () => {
    const input = {
      encoding: "utf8",
      value: "a?b",
      offset: "0",
      ignoreAsciiCase: false,
    } as const;
    expect(
      buildSectorQuery(
        [{ ...input, textWildcard: "?", sectorByte: "0xFFF" }],
        "all",
        4096,
      ).conditions[0],
    ).toEqual({
      pattern: { encoding: "utf8", value: "a?b" },
      offset: 0,
      ignoreAsciiCase: false,
      textWildcard: "?",
      sectorByte: 4095,
    });
    expect(
      buildSectorQuery(
        [{ ...input, textWildcard: "", sectorByte: "  " }],
        "all",
        4096,
      ).conditions[0],
    ).not.toHaveProperty("textWildcard");
    expect(
      buildSectorQuery([{ ...input, sectorByte: "0x0" }], "all", 512)
        .conditions[0].sectorByte,
    ).toBe(0);
    for (const sectorByte of ["4096", "0x1000", "-1", "1e0", "0.5"])
      expect(() =>
        buildSectorQuery([{ ...input, sectorByte }], "any", 4096),
      ).toThrow();
    for (const textWildcard of [" ", "é", "??", "\\"])
      expect(() =>
        buildSectorQuery([{ ...input, textWildcard }], "all", 4096),
      ).toThrow();
    expect(() =>
      buildSectorQuery([{ ...input, sectorByte: "512" }], "any", 512),
    ).toThrow();
  });
  it("stages UTF-8 and UTF-16 LE text at the exact byte without touching neighbors", () => {
    const page = Array(4096).fill(0xcc);
    expect(stageSectorBytes(page, 3, "é\n ", "utf8").slice(2, 8)).toEqual([
      0xcc, 0xc3, 0xa9, 10, 32, 0xcc,
    ]);
    expect(stageSectorBytes(page, 4090, "A😀", "utf16Le").slice(-7)).toEqual([
      0xcc, 0x41, 0, 0x3d, 0xd8, 0, 0xde,
    ]);
    expect(() => stageSectorBytes(page, 4091, "A😀", "utf16Le")).toThrow();
    expect(() => stageSectorBytes(page, 0, "\ud800", "utf8")).toThrow();
    expect(() => stageSectorBytes(page, 0, "", "utf16Le")).toThrow();
    expect(page.every((value) => value === 0xcc)).toBe(true);
  });
  it("accepts hex/decimal and lands byte positions in their containing sector", () => {
    expect(sectorLocation("0x6", "sector", 4096, 512, "start")).toEqual({
      sector: 6,
      byte: 0,
    });
    expect(sectorLocation("0x1ff", "byte", 4096, 512, "start")).toEqual({
      sector: 0,
      byte: 511,
    });
    expect(sectorLocation("4095", "byte", 4096, 512, "start")).toEqual({
      sector: 7,
      byte: 511,
    });
    for (const bad of [
      "8",
      "-1",
      "1.1",
      "1e3",
      "Infinity",
      "0x",
      "9007199254740993",
    ])
      expect(() => sectorLocation(bad, "sector", 4096, 512, "start")).toThrow();
    expect(() => sectorLocation("0", "sector", 4096, 0, "start")).toThrow();
  });
  it("locates the selected unit from either end or from the exact cursor", () => {
    expect(sectorLocation("0", "qword", 16384, 4096, "end")).toEqual({
      sector: 3,
      byte: 4088,
    });
    expect(sectorLocation("2", "word", 16384, 4096, "ahead", 4095)).toEqual({
      sector: 1,
      byte: 3,
    });
    expect(sectorLocation("2", "dword", 16384, 4096, "behind", 4101)).toEqual({
      sector: 0,
      byte: 4093,
    });
    expect(sectorLocation("0x200", "qword", 16384, 4096, "start")).toEqual({
      sector: 1,
      byte: 0,
    });
    expect(() =>
      sectorLocation("1", "byte", 16384, 4096, "ahead", 16383),
    ).toThrow();
    expect(() =>
      sectorLocation("2", "word", 16384, 4096, "behind", 3),
    ).toThrow();
    expect(() => sectorLocation("0", "byte", 16384, 4096, "ahead")).toThrow();
  });
  it("interprets signed/unsigned 64-bit values without losing precision", () => {
    expect(interpretBytes(Array(8).fill(255), 0, true).at(-1)).toEqual({
      bits: 64,
      unsigned: "18446744073709551615",
      signed: "-1",
    });
    expect(interpretBytes([1, 2], 0, true)[1].unsigned).toBe("513");
    expect(interpretBytes([1, 2], 0, false)[1].unsigned).toBe("258");
    expect(interpretBytes([1, 2], 1, true)).toHaveLength(1);
    expect(interpretBytes([1], -1, true)).toEqual([]);
  });
  it("counts differences including unequal lengths", () => {
    expect(differingBytes([1, 2, 3], [1, 2, 3])).toEqual([]);
    expect(differingBytes([1, 2], [3, 2, 0])).toEqual([0, 2]);
  });
});
