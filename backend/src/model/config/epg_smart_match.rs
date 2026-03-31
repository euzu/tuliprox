use std::sync::Arc;
use regex::Regex;
use shared::model::{EpgNamePrefix, EpgSmartMatchConfigDto};
use shared::utils::{default_epg_name_prefix_separator, default_epg_strip, CONSTANTS};
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct EpgSmartMatchConfig {
    pub enabled: bool,
    pub normalize_regex: Arc<Regex>,
    pub strip: Vec<String>,
    pub name_prefix: EpgNamePrefix,
    pub name_prefix_separator: Vec<char>,
    pub fuzzy_matching: bool,
    pub match_threshold: u16,
    pub best_match_threshold: u16,
}

macros::from_impl!(EpgSmartMatchConfig);
impl From<&EpgSmartMatchConfigDto> for EpgSmartMatchConfig {
    fn from(dto: &EpgSmartMatchConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            normalize_regex: match &dto.normalize_regex {
                Some(regex_str) => shared::model::REGEX_CACHE.get_or_compile(regex_str).unwrap_or_else(|e| {
                    log::warn!("Invalid normalize_regex '{regex_str}': {e}, using default");
                    CONSTANTS.re_epg_normalize.clone()
                }),
                None => CONSTANTS.re_epg_normalize.clone(),
            },
            strip: dto
                .strip
                .clone()
                .or_else(default_epg_strip)
                .unwrap_or_default()
                .into_iter()
                .map(|item| item.to_lowercase())
                .collect(),
            name_prefix: dto.name_prefix.clone(),
            name_prefix_separator: dto
                .name_prefix_separator
                .clone()
                .or_else(default_epg_name_prefix_separator)
                .unwrap_or_default(),
            fuzzy_matching: dto.fuzzy_matching,
            match_threshold: dto.match_threshold,
            best_match_threshold: dto.best_match_threshold,
        }
    }
}
