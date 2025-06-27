use shared::utils::CONSTANTS;
use regex::Regex;
use shared::model::{EpgNamePrefix, EpgSmartMatchConfigDto};
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct EpgSmartMatchConfig {
    pub enabled: bool,
    pub normalize_regex: Regex,
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
              Some(regex_str) =>  regex::Regex::new(regex_str).unwrap_or_else(|_| CONSTANTS.re_epg_normalize.clone()),
              None => CONSTANTS.re_epg_normalize.clone(),
            },
            strip: match &dto.strip {
                Some(list) => list.iter().map(|s| s.to_lowercase()).collect(),
                None => ["3840p", "uhd", "fhd", "hd", "sd", "4k", "plus", "raw", "full hd"].iter().map(std::string::ToString::to_string).collect(),
            },
            name_prefix: dto.name_prefix.clone(),
            name_prefix_separator: match &dto.name_prefix_separator {
                None => vec![':', '|', '-'],
                Some(list) => list.clone(),
            },
            fuzzy_matching: dto.fuzzy_matching,
            match_threshold: dto.match_threshold,
            best_match_threshold: dto.best_match_threshold,
        }
    }
}
