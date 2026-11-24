import {
  progressMatches,
  settingsFromProgress,
  type RecoveryProgress,
} from "./recoveryProgressModel";
import { describe, it, expect } from "vitest";
import {
  defaultSignatureSettings,
  signatureRequest,
  previewImageMime,
  previewMediaMime,
} from "./signatureModel";
describe("signature scan request", () => {
  it("keeps ZIP and Office selections separate and includes them in the default set", () => {
    const kinds = ["zip", "docx", "xlsx", "pptx", "docm", "xlsm", "pptm", "odt", "ods", "odp"] as const;
    for (const kind of kinds)
      expect(defaultSignatureSettings.kinds).toContain(kind);
    expect(
      signatureRequest(
        { ...defaultSignatureSettings, kinds: ["docx", "docm", "ods", "odp"] },
        1024,
      ).kinds,
    ).toEqual(["docx", "docm", "ods", "odp"]);
  });
  it("never treats archive or Office candidates as browser images", () => {
    for (const name of ["file.zip", "file.docx", "file.xlsx", "file.pptx"])
      expect(previewImageMime(name)).toBe(null);
  });
  it("converts source-relative MiB ranges and retains an explicit subset", () => {
    expect(
      signatureRequest(
        {
          ...defaultSignatureSettings,
          kinds: ["gif", "wav"],
          customRange: true,
          start: "0.5",
          end: "2",
        },
        4 * 1024 ** 2,
      ),
    ).toEqual({ kinds: ["gif", "wav"], start: 524288, length: 1572864 });
    expect(
      signatureRequest(
        { ...defaultSignatureSettings, customRange: true, start: "1", end: "" },
        2 * 1024 ** 2,
      ).length,
    ).toBe(null);
    expect(
      signatureRequest(
        { ...defaultSignatureSettings, start: "garbage", end: "-1" },
        1024,
      ).start,
    ).toBe(0);
  });
  it("rejects empty, duplicate, reversed, malformed and out-of-source ranges", () => {
    for (const patch of [
      { kinds: [] },
      { kinds: ["png", "png"] as const },
      { customRange: true, start: "2", end: "1" },
      { customRange: true, start: "0", end: "5" },
      { customRange: true, start: "-1" },
      { customRange: true, start: "1e2" },
      { customRange: true, start: "0.0000000001" },
      { customRange: true, start: "4", end: "" },
    ]) {
      expect(() =>
        signatureRequest(
          {
            ...defaultSignatureSettings,
            ...patch,
            kinds: [...(patch.kinds ?? defaultSignatureSettings.kinds)],
          },
          4 * 1024 ** 2,
        ),
      ).toThrow();
    }
    expect(() => signatureRequest(defaultSignatureSettings, 0)).toThrow();
    expect(() =>
      signatureRequest(defaultSignatureSettings, Number.MAX_SAFE_INTEGER + 1),
    ).toThrow();
  });
  it("previews supported image formats without treating audio or suffix lookalikes as images", () => {
    expect(previewImageMime("恢复.GIF")).toBe("image/gif");
    expect(previewImageMime("a.BMP")).toBe("image/bmp");
    expect(previewImageMime("a.webp")).toBe("image/webp");
    expect(previewImageMime("a.jpeg")).toBe("image/jpeg");
    expect(previewImageMime("a.ico")).toBe("image/x-icon");
    expect(previewImageMime("a.tif")).toBe(null);
    expect(previewImageMime("a.cur")).toBe(null);
    expect(previewImageMime("a.wav")).toBe(null);
    expect(previewImageMime("a.png.exe")).toBe(null);
  });
  it("keeps media kinds selectable and uses only supported playback extensions", () => {
    expect(signatureRequest({ ...defaultSignatureSettings, kinds: ["ogg", "mov"] }, 1024).kinds).toEqual(["ogg", "mov"]);
    expect(previewMediaMime("video.MP4")).toBe("video/mp4");
    expect(previewMediaMime("video.mov")).toBe("video/quicktime");
    expect(previewMediaMime("stream.ogg")).toBe("application/ogg");
    expect(previewMediaMime("audio.wav")).toBe("audio/wav");
    expect(previewMediaMime("audio.FLAC")).toBe("audio/flac");
    expect(previewMediaMime("video.mp4.exe")).toBeNull();
  });
});
describe("saved signature scope", () => {
  const saved: RecoveryProgress = {
    id: "s",
    engine: "signatures",
    mode: "signatures",
    source: { target: "B", layoutKey: "layout", partition: 9 },
    options: { kinds: ["pdf", "zip"], start: 12345, length: 76543 },
    done: 0,
    total: 76543,
    found: 0,
    running: false,
    canResume: true,
    canSave: true,
    loaded: true,
  };
  it("never offers another disk, changed layout or whole disk the partition cursor", () => {
    expect(progressMatches(saved, "B", "layout", 9)).toBe(true);
    expect(progressMatches(saved, "A", "layout", 9)).toBe(false);
    expect(progressMatches(saved, "B", "new", 9)).toBe(false);
    expect(progressMatches(saved, "B", "layout", null)).toBe(false);
    expect(progressMatches(null, "B", "layout", 9)).toBe(false);
  });
  it("restores non-MiB boundaries without rounding a byte away", () => {
    for (const start of [1, 12345, 1048575, 2 ** 40 + 137]) {
      const p = { ...saved, options: { ...saved.options!, start } };
      expect(signatureRequest(settingsFromProgress(p), 2 ** 43)).toEqual(
        p.options,
      );
    }
    const p = {
      ...saved,
      options: { ...saved.options!, start: 137, length: null },
    };
    expect(signatureRequest(settingsFromProgress(p), 2 ** 43)).toEqual(
      p.options,
    );
  });
  it("keeps saved free-cluster and sector alignment settings in the request", () => {
    const p = { ...saved, options: { ...saved.options!, freeOnly: true, sectorAligned: true } };
    expect(signatureRequest(settingsFromProgress(p), 2 ** 43)).toEqual(p.options);
  });
  it("restores both fragment options and only sends options for selected types", () => {
    const p: RecoveryProgress = {
      ...saved,
      options: { ...saved.options!, kinds: ["docx", "ogg"], zipFragments: true, oggFragments: true },
    };
    const settings = settingsFromProgress(p);
    expect(signatureRequest(settings, 2 ** 43)).toEqual(p.options);
    const request = signatureRequest({ ...settings, kinds: ["docx"] }, 2 ** 43);
    expect(request.zipFragments).toBe(true);
    expect(request).not.toHaveProperty("oggFragments");
  });
});
