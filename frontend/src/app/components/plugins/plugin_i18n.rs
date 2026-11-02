//! Plugin locale maps: loading, the exact → language → `en` fallback, `t()`.
//!
//! A plugin ships flat string maps at `ui/locales/<lang>.json`. The host loads
//! the maps for the active locale, and uses them for contribution `title_key`s,
//! `SchemaForm` `title` / `description` values that are keys, metric labels, and
//! the `strings` it hands to a frame.

use crate::services::{PluginLocaleMap, PluginUiService};
use serde_json::Value;
use shared::model::{PluginContributionDto, PluginUiDto};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

/// The resolved strings of one plugin for one locale.
pub type PluginStrings = Rc<PluginLocaleMap>;

type CacheKey = (String, String, String);

thread_local! {
    /// Resolved maps per (plugin id, version, language): an upgrade has a new
    /// version and so loads again.
    static LOCALE_CACHE: RefCell<HashMap<CacheKey, PluginStrings>> = RefCell::new(HashMap::new());
}

fn normalized(code: &str) -> String { code.replace('_', "-").to_ascii_lowercase() }

/// The declared locale files to read for `lang`, least specific first: `en`,
/// the language (`pt` for `pt-BR`), the exact code. Reading them in this order
/// and letting later ones win gives the exact → language → `en` fallback per key.
pub fn locale_chain(declared: &[String], lang: &str) -> Vec<String> {
    let exact = normalized(lang);
    let language = exact.split('-').next().unwrap_or_default().to_owned();
    let mut chain = Vec::new();
    for wanted in ["en", language.as_str(), exact.as_str()] {
        if wanted.is_empty() {
            continue;
        }
        if let Some(file) = declared.iter().find(|declared| normalized(declared) == wanted) {
            if !chain.contains(file) {
                chain.push(file.clone());
            }
        }
    }
    chain
}

/// The strings of `plugin` for `lang`, from the cache or the plugin's locale files.
/// A file that cannot be loaded is skipped, so a plugin without usable locale
/// files shows its literal titles.
pub async fn load_plugin_strings(service: &PluginUiService, plugin: &PluginUiDto, lang: &str) -> PluginStrings {
    let key = (plugin.id.clone(), plugin.version.clone(), normalized(lang));
    if let Some(cached) = LOCALE_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return cached;
    }
    let mut merged = PluginLocaleMap::new();
    for file in locale_chain(&plugin.locales, lang) {
        match service.locale(&plugin.id, &plugin.version, &file).await {
            Ok(strings) => merged.extend(strings),
            Err(err) => log::warn!("plugin {}: locale {file} not loaded: {err}", plugin.id),
        }
    }
    let strings: PluginStrings = Rc::new(merged);
    LOCALE_CACHE.with(|cache| cache.borrow_mut().insert(key, Rc::clone(&strings)));
    strings
}

/// Replaces `{name}` placeholders. A placeholder without a parameter stays as written.
#[allow(dead_code)] // host-side counterpart of the frame's `t()`, see `plugin_t`
pub fn interpolate(text: &str, params: &[(&str, &str)]) -> String {
    let mut result = text.to_owned();
    for (name, value) in params {
        result = result.replace(&format!("{{{name}}}"), value);
    }
    result
}

/// `t(key, params)`: the text of `key`, or the key itself when the plugin has none. The frame
/// has its own `t()` in the bridge client; this is the same lookup for host components that
/// render a plugin-owned string with parameters.
#[allow(dead_code)]
pub fn plugin_t(strings: &PluginLocaleMap, key: &str, params: &[(&str, &str)]) -> String {
    strings.get(key).map_or_else(|| key.to_owned(), |text| interpolate(text, params))
}

/// A value that may be a key of the plugin's locale file or a literal text:
/// the translation when it is a key, the value otherwise.
pub fn resolve_text(strings: &PluginLocaleMap, key_or_text: &str) -> String {
    strings.get(key_or_text).cloned().unwrap_or_else(|| key_or_text.to_owned())
}

/// The title of a contribution: its `title_key` in the plugin's strings, else the literal `title`.
pub fn contribution_title(strings: &PluginLocaleMap, contribution: &PluginContributionDto) -> String {
    title_or_key(strings, &contribution.title, contribution.title_key.as_deref())
}

/// A title with an optional key: the key's translation when there is one, else the literal.
pub fn title_or_key(strings: &PluginLocaleMap, title: &str, title_key: Option<&str>) -> String {
    title_key.and_then(|key| strings.get(key)).cloned().unwrap_or_else(|| title.to_owned())
}

/// The schema with every `title` and `description` that is a key of the
/// plugin's strings replaced by its translation, for `SchemaForm`.
pub fn translate_schema(schema: &Value, strings: &PluginLocaleMap) -> Value {
    if strings.is_empty() {
        return schema.clone();
    }
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let translated = match (key.as_str(), value) {
                        ("title" | "description", Value::String(text)) => Value::String(resolve_text(strings, text)),
                        _ => translate_schema(value, strings),
                    };
                    (key.clone(), translated)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|item| translate_schema(item, strings)).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declared(codes: &[&str]) -> Vec<String> { codes.iter().map(ToString::to_string).collect() }

    fn strings(entries: &[(&str, &str)]) -> PluginLocaleMap {
        entries.iter().map(|(key, text)| (key.to_string(), text.to_string())).collect()
    }

    #[test]
    fn chain_is_en_then_language_then_exact() {
        let all = declared(&["en", "pt", "pt-BR", "es"]);
        assert_eq!(locale_chain(&all, "pt-BR"), ["en", "pt", "pt-BR"]);
        assert_eq!(locale_chain(&all, "pt_br"), ["en", "pt", "pt-BR"]);
        assert_eq!(locale_chain(&all, "es"), ["en", "es"]);
        assert_eq!(locale_chain(&all, "en"), ["en"]);
    }

    #[test]
    fn chain_skips_what_the_plugin_does_not_declare() {
        assert_eq!(locale_chain(&declared(&["en", "es"]), "es-MX"), ["en", "es"]);
        assert_eq!(locale_chain(&declared(&["en"]), "ru"), ["en"]);
        assert_eq!(locale_chain(&declared(&["de"]), "fr"), Vec::<String>::new());
        assert_eq!(locale_chain(&[], "en"), Vec::<String>::new());
    }

    #[test]
    fn t_interpolates_and_falls_back_to_the_key() {
        let map = strings(&[("hello", "Hello {name}, you have {count} new")]);
        assert_eq!(plugin_t(&map, "hello", &[("name", "Ana"), ("count", "3")]), "Hello Ana, you have 3 new");
        assert_eq!(plugin_t(&map, "hello", &[("name", "Ana")]), "Hello Ana, you have {count} new");
        assert_eq!(plugin_t(&map, "missing.key", &[("name", "Ana")]), "missing.key");
    }

    #[test]
    fn titles_resolve_their_key_or_keep_the_literal() {
        let map = strings(&[("menu.title", "Guide")]);
        assert_eq!(title_or_key(&map, "Fallback", Some("menu.title")), "Guide");
        assert_eq!(title_or_key(&map, "Fallback", Some("menu.other")), "Fallback");
        assert_eq!(title_or_key(&map, "Fallback", None), "Fallback");
        assert_eq!(resolve_text(&map, "menu.title"), "Guide");
        assert_eq!(resolve_text(&map, "Live matches"), "Live matches");
    }

    #[test]
    fn schema_titles_and_descriptions_that_are_keys_are_translated() {
        let map = strings(&[("f.host", "Server"), ("f.host.help", "Where to connect")]);
        let schema = json!({
            "type": "object",
            "title": "Settings",
            "properties": {
                "host": {"type": "string", "title": "f.host", "description": "f.host.help", "default": "f.host"},
                "title": {"type": "string", "description": "plain text"}
            }
        });
        let translated = translate_schema(&schema, &map);
        assert_eq!(translated["title"], "Settings");
        assert_eq!(translated["properties"]["host"]["title"], "Server");
        assert_eq!(translated["properties"]["host"]["description"], "Where to connect");
        assert_eq!(translated["properties"]["host"]["default"], "f.host");
        assert_eq!(translated["properties"]["title"]["description"], "plain text");
        assert_eq!(translate_schema(&schema, &PluginLocaleMap::new()), schema);
    }
}
