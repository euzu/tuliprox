use i18nrs::{I18n, I18nConfig, StorageType};
use serde_json::Value;
use std::{
    collections::HashMap,
    rc::Rc,
    sync::{Mutex, OnceLock},
};
use yew::prelude::*;

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
    fn from_parts(supported_languages: &[&'static str], translations: &HashMap<String, Value>) -> Self {
        let mut serialized = HashMap::<&'static str, &'static str>::new();
        for lang in supported_languages {
            let value = translations.get(*lang).cloned().unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            serialized.insert(intern_static(lang), intern_string(value.to_string()));
        }

        if serialized.is_empty() {
            serialized.insert(intern_static("en"), intern_static("{}"));
        }

        let config = I18nConfig { translations: serialized.clone() };
        let mut i18n = I18n::new(config, serialized).expect("Failed to initialize i18nrs");
        if let Some(default_lang) = supported_languages.first() {
            let _ = i18n.set_translation_language(default_lang, &StorageType::LocalStorage, "tp_language");
        }

        Self { inner: Rc::new(i18n) }
    }

    pub fn t(&self, key: &str) -> String { self.inner.t(key) }
}

#[derive(Debug, Clone, PartialEq, Properties)]
pub struct I18nProviderProps {
    #[prop_or_else(|| vec!["en", "fr"])]
    pub supported_languages: Vec<&'static str>,
    #[prop_or_default]
    pub translations: HashMap<String, Value>,
    #[prop_or_default]
    pub children: Children,
}

#[function_component(I18nProvider)]
pub fn i18n_provider(props: &I18nProviderProps) -> Html {
    let i18n = use_memo(
        (props.supported_languages.clone(), props.translations.clone()),
        |(supported_languages, translations)| YewI18n::from_parts(supported_languages, translations),
    );

    html! {
        <ContextProvider<YewI18n> context={(*i18n).clone()}>
            { for props.children.iter() }
        </ContextProvider<YewI18n>>
    }
}

#[hook]
pub fn use_translation() -> YewI18n { use_context::<YewI18n>().expect("No I18n context provided") }
