import { createContext, useContext, useEffect, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import type { ReactNode } from "react";
import {
  languagePreference,
  languagePreferenceKey,
  preferredLanguage,
} from "./languages";
import type { Language, LanguagePreference } from "./languages";
import { translate, translateKnown } from "./translate";
import type { Translate } from "./translate";

type Locale = {
  language: Language;
  t: Translate;
  catalogText: (text: string) => string;
};

type LanguageSettings = Locale & {
  preference: LanguagePreference;
  setPreference: (preference: LanguagePreference) => void;
};

const LanguageContext = createContext<LanguageSettings | null>(null);

function storedPreference(): LanguagePreference {
  try {
    return languagePreference(localStorage.getItem(languagePreferenceKey));
  } catch {
    return "system";
  }
}

function systemLanguages(): readonly string[] {
  return [...navigator.languages, navigator.language];
}

export function LanguageProvider({ children }: { children: ReactNode }) {
  const [preference, setPreference] = useState(storedPreference);
  const [system, setSystem] = useState(systemLanguages);
  const language = preferredLanguage(preference, system);

  useEffect(() => {
    const onChange = () => setSystem(systemLanguages());
    window.addEventListener("languagechange", onChange);
    return () => window.removeEventListener("languagechange", onChange);
  }, []);

  useEffect(() => {
    document.documentElement.lang = language;
    document.title = `DiskStudio · ${translate(language, "磁盘管理")}`;
    if (isTauri()) {
      void invoke("set_display_language", { language }).catch(() => {
        /* The WebView can render independently of the native dialog labels. */
      });
    }
  }, [language]);

  useEffect(() => {
    try {
      localStorage.setItem(languagePreferenceKey, preference);
    } catch {
      /* Preferences are optional when storage is unavailable. */
    }
  }, [preference]);

  return (
    <LanguageContext.Provider
      value={{
        language,
        t: (message, variables) => translate(language, message, variables),
        catalogText: (text) => translateKnown(language, text),
        preference,
        setPreference,
      }}
    >
      {children}
    </LanguageContext.Provider>
  );
}

const standaloneLocale: Locale = {
  language: "zh-CN",
  t: (message, variables) => translate("zh-CN", message, variables),
  catalogText: (text) => text,
};

export function useLocale(): Locale {
  return useContext(LanguageContext) ?? standaloneLocale;
}

export function useLanguageSettings(): LanguageSettings {
  const settings = useContext(LanguageContext);
  if (!settings) throw new Error("LanguageProvider is required");
  return settings;
}
