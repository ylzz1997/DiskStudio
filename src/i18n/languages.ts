export const languages = {
  "zh-CN": "简体中文",
  "zh-TW": "繁體中文",
  en: "English",
  ja: "日本語",
  ko: "한국어",
  pt: "Português",
  es: "Español",
  de: "Deutsch",
  fr: "Français",
  it: "Italiano",
  ru: "Русский",
} as const;

export type Language = keyof typeof languages;
export type LanguagePreference = Language | "system";

export const languagePreferenceKey = "diskstudio.language";

function isLanguage(value: string): value is Language {
  return Object.hasOwn(languages, value);
}

export function languagePreference(value: string | null): LanguagePreference {
  return value && isLanguage(value) ? value : "system";
}

export function preferredLanguage(
  preference: LanguagePreference,
  systemLanguages: readonly string[],
): Language {
  if (preference !== "system") return preference;

  for (const name of systemLanguages) {
    try {
      const { language, script, region } = new Intl.Locale(name);
      if (language === "zh") {
        const traditional = script
          ? script === "Hant"
          : region === "TW" || region === "HK" || region === "MO";
        return traditional ? "zh-TW" : "zh-CN";
      }
      if (isLanguage(language)) return language;
    } catch {
      // Invalid tags from the environment do not override later preferences.
    }
  }
  return "en";
}
