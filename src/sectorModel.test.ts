import { describe, it, expect } from "vitest";
import {
  sectorPosition,
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
  it("accepts hex/decimal and lands byte positions in their containing sector", () => {
    expect(sectorPosition("0x6", "sector", 4096, 512)).toBe(6);
    expect(sectorPosition("0x1ff", "byte", 4096, 512)).toBe(0);
    expect(sectorPosition("4095", "byte", 4096, 512)).toBe(7);
    for (const bad of [
      "8",
      "-1",
      "1.1",
      "1e3",
      "Infinity",
      "0x",
      "9007199254740993",
    ])
      expect(() => sectorPosition(bad, "sector", 4096, 512)).toThrow();
    expect(() => sectorPosition("0", "sector", 4096, 0)).toThrow();
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
