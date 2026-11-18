import de from "./messages/de.json";
import en from "./messages/en.json";
import es from "./messages/es.json";
import fr from "./messages/fr.json";
import it from "./messages/it.json";
import ja from "./messages/ja.json";
import ko from "./messages/ko.json";
import pt from "./messages/pt.json";
import ru from "./messages/ru.json";
import zhTW from "./messages/zh-TW.json";
import nativeDiagnosticFallback from "./nativeDiagnosticFallback.json";
import type { Language } from "./languages";

export type Message = keyof typeof en;
type Variables = Readonly<Record<string, string | number>>;
type Translations = Record<Message, string>;

const dictionaries: Record<Exclude<Language, "zh-CN">, Translations> = {
  de,
  en,
  es,
  fr,
  it,
  ja,
  ko,
  pt,
  ru,
  "zh-TW": zhTW,
};

export const messages = Object.keys(en) as Message[];

export function translate(
  language: Language,
  message: Message,
  variables?: Variables,
): string {
  const text =
    language === "zh-CN"
      ? message
      : (dictionaries[language][message] ?? en[message] ?? message);
  if (!variables) return text;
  return text.replace(/\{([a-zA-Z][\w]*)\}/g, (part, name: string) =>
    Object.hasOwn(variables, name) ? String(variables[name]) : part,
  );
}

export function translateKnown(language: Language, text: string): string {
  if (Object.hasOwn(en, text)) return translate(language, text as Message);
  if (language !== "zh-CN" && Object.hasOwn(nativeDiagnosticFallback, text)) {
    return nativeDiagnosticFallback[text as keyof typeof nativeDiagnosticFallback];
  }
  return text;
}

export type Translate = (message: Message, variables?: Variables) => string;

export class MessageError extends Error {
  constructor(
    readonly key: Message,
    readonly variables?: Variables,
  ) {
    super(translate("zh-CN", key, variables));
  }
}
