use crate::utils::is_blank_optional_string;
use std::fmt::Display;
use log::warn;
use crate::error::{info_err_res, TuliproxError};
use crate::utils::{is_false, default_match_threshold, default_best_match_threshold,
                   is_default_match_threshold, is_default_best_match_threshold};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum EpgNamePrefix {
    #[default]
    Ignore,
    Suffix(String),
    Prefix(String),
}

impl EpgNamePrefix {
    const IGNORE: &'static str = "Ignore";
    const SUFFIX: &'static str = "Suffix";
    const PREFIX: &'static str = "Prefix";
}

impl Display for EpgNamePrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Ignore => write!(f, "{}", Self::IGNORE),
            Self::Suffix(s) => write!(f, "{}({s})", Self::SUFFIX),
            Self::Prefix(s) => write!(f, "{}({s})", Self::PREFIX),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EpgSmartMatchConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub normalize_regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip: Option<Vec<String>>,
    #[serde(default)]
    pub name_prefix: EpgNamePrefix,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_prefix_separator: Option<Vec<char>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub fuzzy_matching: bool,
    #[serde(default = "default_match_threshold", skip_serializing_if = "is_default_match_threshold")]
    pub match_threshold: u16,
    #[serde(default = "default_best_match_threshold", skip_serializing_if = "is_default_best_match_threshold")]
    pub best_match_threshold: u16,
}
impl Default for EpgSmartMatchConfigDto {
    fn default() -> Self {
        EpgSmartMatchConfigDto {
            enabled: false,
            normalize_regex: None,
            strip: None,
            name_prefix: EpgNamePrefix::default(),
            name_prefix_separator: None,
            fuzzy_matching: false,
            match_threshold: default_match_threshold(),
            best_match_threshold: default_best_match_threshold(),
        }
    }
}

impl EpgSmartMatchConfigDto {

    /// # Panics
    ///
    /// Prepares the EPG smart match configuration by validating thresholds, compiling normalization regex, and setting default values as needed.
    ///
    /// Adjusts match thresholds to valid ranges, compiles the normalization regex, and sets default strip values and name prefix separators if not provided. Returns an error if the normalization regex is invalid.
    ///
    /// # Returns
    ///
    /// `Ok(())` if preparation succeeds, or an `TuliproxError` if regex compilation fails.
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if !self.enabled {
            return Ok(());
        }

        if self.match_threshold == 0 {
            self.match_threshold = 80;
        } else if self.match_threshold < 10 {
            warn!("the match_threshold is less than 10%, set to 10%");
            self.match_threshold = 10;
        } else if self.match_threshold > 100 {
            warn!("the match_threshold is more than 100%, set to 80%");
            self.match_threshold = 100;
        }

        if self.best_match_threshold == 0 || self.best_match_threshold > 100 || self.best_match_threshold < self.match_threshold {
            self.best_match_threshold = 99;
        }

        if let Some(regstr) = self.normalize_regex.as_ref() {
            let re = crate::model::REGEX_CACHE.get_or_compile(regstr.as_str());
            if re.is_err() {
                return info_err_res!("can't parse regex: {}", regstr);
            }
        };

        Ok(())
    }
}