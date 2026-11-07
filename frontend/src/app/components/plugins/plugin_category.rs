//! The name of a plugin category in the Web UI.
//!
//! A category is an id (`notifier`, `epg`, `live-tv`). The ids the UI knows
//! (`KNOWN_PLUGIN_CATEGORIES`) have a translated label each; any other valid id, which a
//! catalog may use, is shown as written, with its words capitalised.

use crate::i18n::YewI18n;
use shared::model::KNOWN_PLUGIN_CATEGORIES;

/// Words that are acronyms and are written in capitals.
const ACRONYMS: [&str; 9] = ["tv", "ai", "api", "dvr", "epg", "hls", "iptv", "ui", "vod"];

/// `live-tv` as `Live TV`: the words of an id, capitalised, for a category with no translation. A few
/// well-known acronyms are written in capitals.
pub fn humanize_category(id: &str) -> String {
    id.split('-')
        .filter(|word| !word.is_empty())
        .map(|word| {
            if ACRONYMS.contains(&word) {
                return word.to_uppercase();
            }
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The i18n key of a known category: `notifier` is `LABEL.PLUGIN_CATEGORY_NOTIFIER`.
fn label_key(id: &str) -> String { format!("LABEL.PLUGIN_CATEGORY_{}", id.to_uppercase().replace('-', "_")) }

/// The name of the category `id` in the active language.
pub fn category_label(translate: &YewI18n, id: &str) -> String {
    if KNOWN_PLUGIN_CATEGORIES.contains(&id) {
        translate.t(&label_key(id))
    } else {
        humanize_category(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_category_is_shown_as_capitalised_words() {
        assert_eq!(humanize_category("live-tv"), "Live TV");
        assert_eq!(humanize_category("epg"), "EPG");
        assert_eq!(humanize_category("iptv-tools"), "IPTV Tools");
        assert_eq!(humanize_category("tvdb"), "Tvdb", "only whole words are acronyms");
        assert_eq!(humanize_category("home-automation-2"), "Home Automation 2");
        assert_eq!(humanize_category("-x--y-"), "X Y");
        assert_eq!(humanize_category(""), "");
    }

    #[test]
    fn the_label_key_of_a_known_category_is_its_upper_case_id() {
        assert_eq!(label_key("notifier"), "LABEL.PLUGIN_CATEGORY_NOTIFIER");
        assert_eq!(label_key("live-tv"), "LABEL.PLUGIN_CATEGORY_LIVE_TV");
    }

    #[test]
    fn every_known_category_has_a_translation_in_every_language() {
        for language in ["en", "ru", "es", "ar"] {
            let text = std::fs::read_to_string(format!("public/assets/i18n/{language}.json")).expect("language file");
            let strings: serde_json::Value = serde_json::from_str(&text).expect("valid json");
            for id in KNOWN_PLUGIN_CATEGORIES {
                let key = label_key(id);
                let name = key.trim_start_matches("LABEL.");
                let label = strings["LABEL"][name].as_str().unwrap_or_default();
                assert!(!label.trim().is_empty(), "{language}: `{key}` is missing");
            }
        }
    }
}
