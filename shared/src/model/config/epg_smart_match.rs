use log::warn;
use crate::error::{create_tuliprox_error_result, TuliproxError, TuliproxErrorKind};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum EpgNamePrefix {
    #[default]
    Ignore,
    Suffix(String),
    Prefix(String),
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpgSmartMatchConfigDto {
    #[serde(default)]
    pub enabled: bool,
    pub normalize_regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip: Option<Vec<String>>,
    #[serde(default)]
    pub name_prefix: EpgNamePrefix,
    #[serde(default)]
    pub name_prefix_separator: Option<Vec<char>>,
    #[serde(default)]
    pub fuzzy_matching: bool,
    #[serde(default)]
    pub match_threshold: u16,
    #[serde(default)]
    pub best_match_threshold: u16,
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
            let re = regex::Regex::new(regstr.as_str());
            if re.is_err() {
                return create_tuliprox_error_result!(TuliproxErrorKind::Info, "cant parse regex: {}", regstr);
            }
        };

        Ok(())
    }
}