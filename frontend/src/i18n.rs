use i18nrs::{I18n, I18nConfig, StorageType};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    rc::Rc,
    sync::{Mutex, OnceLock},
};
use yew::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct LanguageInfo {
    pub code: String,
    #[serde(default)]
    pub label: String,
    /// Text direction: "ltr" (default) or "rtl" (e.g. Arabic).
    #[serde(default = "default_dir")]
    pub dir: String,
}

fn default_dir() -> String { "ltr".to_string() }

impl LanguageInfo {
    pub fn is_rtl(&self) -> bool { self.dir.eq_ignore_ascii_case("rtl") }

    pub fn display_label(&self) -> String {
        if self.label.is_empty() {
            self.code.to_uppercase()
        } else {
            self.label.clone()
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct LanguageManifest {
    #[serde(default)]
    pub languages: Vec<LanguageInfo>,
}

#[derive(Clone, PartialEq)]
pub struct LanguageState {
    pub languages: Rc<Vec<LanguageInfo>>,
    pub active: String,
    pub on_change: Callback<String>,
}

static INTERNED_STRINGS: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();

fn intern_owned(cache: &mut HashMap<&'static str, &'static str>, value: String) -> &'static str {
    let leaked = Box::leak(value.into_boxed_str());
    cache.insert(leaked, leaked);
    leaked
}

fn intern_static(value: &str) -> &'static str {
    let cache = INTERNED_STRINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("i18n intern cache poisoned");
    if let Some(existing) = cache.get(value).copied() {
        return existing;
    }
    intern_owned(&mut cache, value.to_owned())
}

fn intern_string(value: String) -> &'static str {
    let cache = INTERNED_STRINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("i18n intern cache poisoned");
    if let Some(existing) = cache.get(value.as_str()).copied() {
        return existing;
    }
    intern_owned(&mut cache, value)
}

#[derive(Clone)]
pub struct YewI18n {
    inner: Rc<I18n>,
}

impl PartialEq for YewI18n {
    fn eq(&self, other: &Self) -> bool { Rc::ptr_eq(&self.inner, &other.inner) }
}

impl YewI18n {
    fn from_parts(
        supported_languages: &[String],
        active_language: &str,
        translations: &HashMap<String, Value>,
    ) -> Self {
        let mut serialized = HashMap::<&'static str, &'static str>::new();
        for lang in supported_languages {
            let value = translations.get(lang).cloned().unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            serialized.insert(intern_static(lang), intern_string(value.to_string()));
        }

        if serialized.is_empty() {
            serialized.insert(intern_static("en"), intern_static("{}"));
        }

        let config = I18nConfig { translations: serialized.clone() };
        let mut i18n = I18n::new(config, serialized).expect("Failed to initialize i18nrs");
        let default_lang =
            supported_languages.iter().find(|l| l.as_str() == active_language).or_else(|| supported_languages.first());
        if let Some(default_lang) = default_lang {
            let _ = i18n.set_translation_language(default_lang, &StorageType::LocalStorage, "tp_language");
        }

        Self { inner: Rc::new(i18n) }
    }

    pub fn t(&self, key: &str) -> String { self.inner.t(key) }
}

#[derive(Debug, Clone, PartialEq, Properties)]
pub struct I18nProviderProps {
    #[prop_or_else(|| vec!["en".to_string()])]
    pub supported_languages: Vec<String>,
    #[prop_or_else(|| "en".to_string())]
    pub active_language: String,
    #[prop_or_default]
    pub translations: HashMap<String, Value>,
    #[prop_or_default]
    pub children: Children,
}

#[function_component(I18nProvider)]
pub fn i18n_provider(props: &I18nProviderProps) -> Html {
    let i18n = use_memo(
        (props.supported_languages.clone(), props.active_language.clone(), props.translations.clone()),
        |(supported_languages, active_language, translations)| {
            YewI18n::from_parts(supported_languages, active_language, translations)
        },
    );

    html! {
        <ContextProvider<YewI18n> context={(*i18n).clone()}>
            { for props.children.iter() }
        </ContextProvider<YewI18n>>
    }
}

#[hook]
pub fn use_translation() -> YewI18n { use_context::<YewI18n>().expect("No I18n context provided") }

#[cfg(test)]
mod locale_file_tests {
    use serde::de::{Deserializer, MapAccess, Visitor};
    use std::{collections::BTreeSet, fmt};

    const LOCALES: [(&str, &str); 4] = [
        ("en", include_str!("../public/assets/i18n/en.json")),
        ("ru", include_str!("../public/assets/i18n/ru.json")),
        ("ar", include_str!("../public/assets/i18n/ar.json")),
        ("es", include_str!("../public/assets/i18n/es.json")),
    ];

    /// The dotted key of every translated string, refusing a key that appears
    /// twice in one object: JSON keeps only the last one, so the first is dead
    /// text that looks translated.
    struct Keys(BTreeSet<String>);

    impl<'de> serde::Deserialize<'de> for Keys {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct KeysVisitor;
            impl<'de> Visitor<'de> for KeysVisitor {
                type Value = Keys;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("a translation object") }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Keys, A::Error> {
                    let mut keys = BTreeSet::new();
                    let mut own = BTreeSet::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if !own.insert(key.clone()) {
                            return Err(serde::de::Error::custom(format!("duplicate key {key}")));
                        }
                        match map.next_value::<serde_json::Value>()? {
                            serde_json::Value::Object(nested) => {
                                let nested: Keys = serde_json::from_value(serde_json::Value::Object(nested))
                                    .map_err(serde::de::Error::custom)?;
                                keys.extend(nested.0.into_iter().map(|child| format!("{key}.{child}")));
                            }
                            _ => {
                                keys.insert(key);
                            }
                        }
                    }
                    Ok(Keys(keys))
                }
            }
            deserializer.deserialize_map(KeysVisitor)
        }
    }

    #[test]
    fn the_checked_locales_are_the_ones_the_ui_offers() {
        let index: serde_json::Value =
            serde_json::from_str(include_str!("../public/assets/i18n/index.json")).expect("index.json");
        let mut offered: Vec<&str> = index["languages"]
            .as_array()
            .expect("languages")
            .iter()
            .filter_map(|language| language["code"].as_str())
            .collect();
        let mut checked: Vec<&str> = LOCALES.iter().map(|(code, _)| *code).collect();
        offered.sort_unstable();
        checked.sort_unstable();
        assert_eq!(checked, offered, "add the new locale to LOCALES");
    }

    #[test]
    fn every_locale_translates_exactly_the_english_keys_once() {
        // A missing key shows the raw key in the UI.
        let english = serde_json::from_str::<Keys>(LOCALES[0].1).expect("en.json").0;
        for (code, text) in LOCALES {
            let keys = serde_json::from_str::<Keys>(text).unwrap_or_else(|err| panic!("{code}.json: {err}")).0;
            let missing: Vec<_> = english.difference(&keys).collect();
            let extra: Vec<_> = keys.difference(&english).collect();
            assert!(missing.is_empty(), "{code}.json is missing {missing:?}");
            assert!(extra.is_empty(), "{code}.json has keys en.json does not: {extra:?}");
        }
    }
}
