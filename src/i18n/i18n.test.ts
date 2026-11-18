import { describe, expect, it } from "vitest";
import { languagePreference, languages, preferredLanguage } from "./languages";
import type { Language } from "./languages";
import { messages, translate, translateKnown } from "./translate";

describe("language preferences", () => {
  it("uses a supported choice regardless of the system language", () => {
    expect(preferredLanguage(languagePreference("ja"), ["en-US"])).toBe("ja");
    expect(languagePreference("__proto__")).toBe("system");
    expect(languagePreference(null)).toBe("system");
  });

  it("selects the Chinese script before inferring it from the region", () => {
    expect(preferredLanguage("system", ["zh-Hant-CN"])).toBe("zh-TW");
    expect(preferredLanguage("system", ["zh-Hans-HK"])).toBe("zh-CN");
    expect(preferredLanguage("system", ["zh-MO"])).toBe("zh-TW");
    expect(preferredLanguage("system", ["zh-SG"])).toBe("zh-CN");
  });

  it("considers every system preference before falling back to English", () => {
    expect(preferredLanguage("system", ["xx_BAD", "ar-SA", "pt-BR"])).toBe(
      "pt",
    );
    expect(preferredLanguage("system", ["ar-SA"])).toBe("en");
    expect(preferredLanguage("system", [])).toBe("en");
  });
});

describe("localized text", () => {
  it("keeps the source wording in Simplified Chinese", () => {
    expect(translate("zh-CN", "磁盘管理")).toBe("磁盘管理");
    expect(translate("zh-TW", "磁盘管理")).toBe("磁碟管理");
    expect(translate("en", "磁盘管理")).toBe("Disk Management");
  });

  it("preserves named parameters in every translation", () => {
    const fields = (value: string) =>
      [...value.matchAll(/\{([a-zA-Z][\w]*)\}/g)].map((part) => part[1]).sort();
    for (const language of Object.keys(languages) as Language[]) {
      for (const message of messages) {
        const text = translate(language, message);
        expect(text.trim(), `${language} ${message}`).not.toBe("");
        expect(fields(text), `${language} ${message}`).toEqual(fields(message));
      }
    }
  });

  it("formats parameter data without treating it as another template", () => {
    expect(translate("en", "上次读取 {time}", { time: "11:30 {time}" })).toBe(
      "Last read 11:30 {time}",
    );
  });

  it("falls back to English for known diagnostics and preserves unknown details", () => {
    expect(translateKnown("ja", "原生设备状态与 WMI 不一致")).toBe(
      "The native device state disagrees with WMI.",
    );
    expect(translateKnown("zh-CN", "原生设备状态与 WMI 不一致")).toBe(
      "原生设备状态与 WMI 不一致",
    );
    expect(translateKnown("ja", "Device status 0xC0000001")).toBe(
      "Device status 0xC0000001",
    );
  });
});
