use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    En,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "zh-TW")]
    ZhTw,
    Ja,
    Ko,
    Pt,
    Es,
    De,
    Fr,
    It,
    Ru,
}

impl Language {
    pub fn tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh-CN",
            Self::ZhTw => "zh-TW",
            Self::Ja => "ja",
            Self::Ko => "ko",
            Self::Pt => "pt",
            Self::Es => "es",
            Self::De => "de",
            Self::Fr => "fr",
            Self::It => "it",
            Self::Ru => "ru",
        }
    }

    fn catalog(self) -> &'static str {
        match self {
            Self::En => include_str!("../../src/i18n/messages/en.json"),
            Self::ZhCn => "{}",
            Self::ZhTw => include_str!("../../src/i18n/messages/zh-TW.json"),
            Self::Ja => include_str!("../../src/i18n/messages/ja.json"),
            Self::Ko => include_str!("../../src/i18n/messages/ko.json"),
            Self::Pt => include_str!("../../src/i18n/messages/pt.json"),
            Self::Es => include_str!("../../src/i18n/messages/es.json"),
            Self::De => include_str!("../../src/i18n/messages/de.json"),
            Self::Fr => include_str!("../../src/i18n/messages/fr.json"),
            Self::It => include_str!("../../src/i18n/messages/it.json"),
            Self::Ru => include_str!("../../src/i18n/messages/ru.json"),
        }
    }
}

type Catalog = HashMap<String, String>;
static ENGLISH: LazyLock<Catalog> =
    LazyLock::new(|| serde_json::from_str(Language::En.catalog()).unwrap_or_default());
static DIAGNOSTIC_FALLBACK: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../src/i18n/nativeDiagnosticFallback.json"))
        .expect("valid English diagnostic fallback catalog")
});

#[derive(Clone)]
pub struct Locale {
    language: Language,
    messages: Arc<Catalog>,
}

impl Locale {
    pub fn new(language: Language) -> Self {
        let messages = match language {
            Language::En | Language::ZhCn => HashMap::new(),
            _ => serde_json::from_str(language.catalog()).unwrap_or_default(),
        };
        Self {
            language,
            messages: Arc::new(messages),
        }
    }

    pub fn tag(&self) -> &'static str {
        self.language.tag()
    }

    pub fn text<'a>(&'a self, source: &'a str) -> &'a str {
        if self.language == Language::ZhCn {
            return source;
        }
        self.messages
            .get(source)
            .or_else(|| ENGLISH.get(source))
            .or_else(|| DIAGNOSTIC_FALLBACK.get(source))
            .map(String::as_str)
            .unwrap_or(source)
    }

    pub fn sentences(&self, sources: &[&str]) -> String {
        sources
            .iter()
            .filter(|source| !source.is_empty())
            .map(|source| self.text(source))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn format(&self, source: &str, values: &[(&str, &str)]) -> String {
        let mut remaining = self.text(source);
        let mut result = String::with_capacity(remaining.len());
        while let Some((before, parameter)) = remaining.split_once('{') {
            let Some((name, after)) = parameter.split_once('}') else {
                break;
            };
            result.push_str(before);
            if let Some((_, value)) = values.iter().find(|(key, _)| *key == name) {
                result.push_str(value);
            } else {
                result.push('{');
                result.push_str(name);
                result.push('}');
            }
            remaining = after;
        }
        result.push_str(remaining);
        result
    }
}

static CURRENT: LazyLock<RwLock<Locale>> = LazyLock::new(|| RwLock::new(Locale::new(Language::En)));

pub fn current() -> Locale {
    CURRENT
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

pub fn set(language: Language) {
    let mut current = CURRENT.write().unwrap_or_else(|err| err.into_inner());
    if current.language != language {
        *current = Locale::new(language);
    }
}

#[cfg(windows)]
pub fn text(source: &str) -> String {
    current().text(source).to_owned()
}

#[cfg(windows)]
pub fn diagnostic(source: &str, detail: &str) -> String {
    let locale = current();
    locale.format(source, &[("error", locale.text(detail))])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_locale_explains_existing_output_in_english() {
        assert_eq!(
            Locale::new(Language::En).text("目标已存在；不会覆盖已有文件。"),
            "That file already exists. Choose a new filename."
        );
    }

    #[test]
    fn native_locale_formats_user_data_only_once() {
        let locale = Locale::new(Language::En);
        let path = r"D:\资料\{path}.html";
        assert_eq!(
            locale.format("输出目录 {path}", &[("path", path)]),
            format!("Output folder {path}")
        );
        assert_eq!(
            locale.format(
                "文件 {id}：{error}",
                &[("error", "detail {id}"), ("id", "7")]
            ),
            "File 7: detail {id}"
        );
    }
}
