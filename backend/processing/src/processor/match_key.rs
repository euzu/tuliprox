use regex::Regex;
use shared::{
    model::{ChannelMatchBy, ChannelMatchNormalize, DeduplicateMatchBy, PlaylistItemHeader},
    utils::{deunicode_string, quality_tokens, token_quality},
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

pub(in crate::processor) const fn dedup_match_by(value: DeduplicateMatchBy) -> ChannelMatchBy {
    match value {
        DeduplicateMatchBy::Caption => ChannelMatchBy::Caption,
        DeduplicateMatchBy::Name => ChannelMatchBy::Name,
        DeduplicateMatchBy::Title => ChannelMatchBy::Title,
    }
}

/// Lowercased token join with quality tokens removed, so "News HD" and
/// "NEWS [FHD]" produce the same key.
pub(in crate::processor) fn normalized_match_key(value: &str, match_as_ascii: bool) -> String {
    let value = if match_as_ascii { deunicode_string(value) } else { std::borrow::Cow::Borrowed(value) };
    let mut key = String::with_capacity(value.len());
    for token in quality_tokens(&value) {
        if token_quality(token).is_some() {
            continue;
        }
        if !key.is_empty() {
            key.push(' ');
        }
        for ch in token.chars() {
            key.extend(ch.to_lowercase());
        }
    }
    key
}

pub(in crate::processor) fn raw_match_value(header: &PlaylistItemHeader, match_by: ChannelMatchBy) -> &str {
    match match_by {
        ChannelMatchBy::Caption => {
            if header.title.is_empty() {
                header.name.as_ref()
            } else {
                header.title.as_ref()
            }
        }
        ChannelMatchBy::Name => header.name.as_ref(),
        ChannelMatchBy::Title => header.title.as_ref(),
        ChannelMatchBy::EpgChannelId => header.epg_channel_id.as_deref().unwrap_or(""),
    }
}

pub(in crate::processor) fn extract_normalized_match_key(
    header: &PlaylistItemHeader,
    match_by: ChannelMatchBy,
    match_as_ascii: bool,
    normalizer: Option<&MatchKeyNormalizer<'_>>,
) -> String {
    let raw = raw_match_value(header, match_by);
    if match_by == ChannelMatchBy::EpgChannelId {
        return raw.trim().to_lowercase();
    }
    match normalizer {
        Some(normalizer) => normalizer.key(raw),
        None => normalized_match_key(raw, match_as_ascii),
    }
}

#[allow(dead_code)]
pub(in crate::processor) struct MatchKeyNormalizer<'a> {
    strip: Option<&'a Regex>,
    ignore: HashSet<String>,
    aliases: HashMap<String, String>,
    match_as_ascii: bool,
}

#[allow(dead_code)]
impl<'a> MatchKeyNormalizer<'a> {
    pub(in crate::processor) fn new(config: &'a ChannelMatchNormalize, match_as_ascii: bool) -> Self {
        let mut normalizer = Self {
            strip: config.t_strip_regex.as_deref(),
            ignore: config.ignore_tokens.iter().map(|token| token.to_lowercase()).collect(),
            aliases: HashMap::new(),
            match_as_ascii,
        };
        let aliases =
            config.aliases.iter().map(|(from, to)| (normalizer.base_key(from), normalizer.base_key(to))).collect();
        normalizer.aliases = aliases;
        normalizer
    }

    pub(in crate::processor) fn key(&self, value: &str) -> String {
        let key = self.base_key(value);
        self.aliases.get(&key).cloned().unwrap_or(key)
    }

    fn base_key(&self, value: &str) -> String {
        let stripped = self.strip.map_or(Cow::Borrowed(value), |regex| regex.replace_all(value, ""));
        let folded = if self.match_as_ascii { deunicode_string(&stripped) } else { stripped };
        let mut key = String::with_capacity(folded.len());
        for token in quality_tokens(&folded) {
            let lower = token.to_lowercase();
            if token_quality(token).is_some() || self.ignore.contains(&lower) {
                continue;
            }
            if !key.is_empty() {
                key.push(' ');
            }
            key.push_str(&lower);
        }
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{
        model::{ChannelGroupsConfig, ChannelMatchNormalize, PlaylistItemHeader},
        utils::Internable,
    };

    fn header(title: &str, name: &str, epg: Option<&str>) -> PlaylistItemHeader {
        PlaylistItemHeader {
            title: title.intern(),
            name: name.intern(),
            epg_channel_id: epg.map(Internable::intern),
            ..Default::default()
        }
    }

    #[test]
    fn strips_quality_tokens_and_lowercases() {
        assert_eq!(normalized_match_key("News HD", false), "news");
        assert_eq!(normalized_match_key("NEWS [FHD]", false), normalized_match_key("news 1080p", false));
    }

    #[test]
    fn caption_falls_back_to_name() {
        assert_eq!(
            extract_normalized_match_key(&header("", "Sport HD", None), ChannelMatchBy::Caption, false, None),
            "sport"
        );
    }

    #[test]
    fn epg_channel_id_is_used_verbatim_lowercased() {
        let h = header("ZDF HD", "ZDF", Some("ZDF.de"));
        assert_eq!(extract_normalized_match_key(&h, ChannelMatchBy::EpgChannelId, false, None), "zdf.de");
        assert_eq!(
            extract_normalized_match_key(&header("X", "X", None), ChannelMatchBy::EpgChannelId, false, None),
            ""
        );
    }

    #[test]
    fn ascii_folding() {
        assert_eq!(normalized_match_key("Café HD", true), normalized_match_key("Cafe FHD", true));
    }

    fn normalize(strip: Option<&str>, ignore: &[&str], aliases: &[(&str, &str)]) -> ChannelMatchNormalize {
        let mut cfg = ChannelGroupsConfig {
            match_normalize: ChannelMatchNormalize {
                strip_regex: strip.map(str::to_string),
                ignore_tokens: ignore.iter().map(|t| (*t).to_string()).collect(),
                aliases: aliases.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
                ..Default::default()
            },
            ..ChannelGroupsConfig::default()
        };
        cfg.prepare().expect("valid");
        cfg.match_normalize
    }

    #[test]
    fn normalizer_strips_prefix_ignores_tokens_and_resolves_aliases() {
        let cfg = normalize(
            Some(r"^\s*(?:\|[A-Z]{2,3}\||[A-Z]{2,3}\s*[:|])\s*"),
            &["HEVC", "50fps"],
            &[("Das Erste HD", "ARD")],
        );
        let normalizer = MatchKeyNormalizer::new(&cfg, false);
        assert_eq!(normalizer.key("DE: ZDF HEVC"), "zdf");
        assert_eq!(normalizer.key("|DE| ZDF 50FPS"), "zdf");
        assert_eq!(normalizer.key("ZDF"), "zdf");
        assert_eq!(normalizer.key("ZDF NEO"), "zdf neo");
        assert_eq!(normalizer.key("DE: Das Erste"), "ard");
    }

    #[test]
    fn dedup_keys_unchanged_without_normalizer() {
        let h = header("DE: ZDF HEVC", "", None);
        assert_eq!(extract_normalized_match_key(&h, ChannelMatchBy::Caption, false, None), "de zdf hevc");
    }
}
