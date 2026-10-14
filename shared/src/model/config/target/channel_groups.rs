use crate::error::TuliproxError;
use regex::Regex;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelMatchBy {
    #[default]
    Caption,
    Name,
    Title,
    EpgChannelId,
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelGroupPolicy {
    #[default]
    Auto,
    Priority,
}

const fn default_true() -> bool { true }
const fn default_min_members() -> u16 { 2 }

#[derive(Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChannelGroupSelectionConfig {
    pub circuit_failures: u8,
    pub circuit_cooldown_secs: u64,
    pub circuit_cooldown_max_secs: u64,
    pub input_circuit_failures: u8,
    pub input_failure_window_secs: u64,
    pub startup_budget_secs: u64,
    pub max_start_attempts: u8,
    pub same_member_retries: u8,
    pub stall_timeout_secs: u64,
    /// 0 disables the underrun trigger.
    pub underrun_ratio_pct: u8,
    pub underrun_window_secs: u64,
    pub runtime_switch_budget_secs: u64,
    pub stable_reset_secs: u64,
    pub flap_window_secs: u64,
    /// 0 disables the flap guard.
    pub flap_max_switches: u8,
    pub runtime_audio_language: RuntimeAudioLanguage,
    pub early_start_response: bool,
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAudioLanguage {
    #[default]
    Prefer,
    Require,
}

impl Default for ChannelGroupSelectionConfig {
    fn default() -> Self {
        Self {
            circuit_failures: 2,
            circuit_cooldown_secs: 60,
            circuit_cooldown_max_secs: 900,
            input_circuit_failures: 3,
            input_failure_window_secs: 120,
            startup_budget_secs: 15,
            max_start_attempts: 4,
            same_member_retries: 1,
            stall_timeout_secs: 8,
            underrun_ratio_pct: 90,
            underrun_window_secs: 15,
            runtime_switch_budget_secs: 20,
            stable_reset_secs: 30,
            flap_window_secs: 600,
            flap_max_switches: 6,
            runtime_audio_language: RuntimeAudioLanguage::Prefer,
            early_start_response: false,
        }
    }
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpgConflictMode {
    #[default]
    Split,
    Ignore,
}

/// Grouping-only match key extensions (design §4.1); `deduplicate` never uses them.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChannelMatchNormalize {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strip_regex: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignore_tokens: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    #[serde(skip)]
    pub t_strip_regex: Option<Arc<Regex>>,
}

impl PartialEq for ChannelMatchNormalize {
    fn eq(&self, other: &Self) -> bool {
        self.strip_regex == other.strip_regex
            && self.ignore_tokens == other.ignore_tokens
            && self.aliases == other.aliases
    }
}

impl Eq for ChannelMatchNormalize {}

impl ChannelMatchNormalize {
    pub fn is_empty(&self) -> bool {
        self.strip_regex.is_none() && self.ignore_tokens.is_empty() && self.aliases.is_empty()
    }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.t_strip_regex = self
            .strip_regex
            .as_deref()
            .map(|pattern| {
                Regex::new(pattern).map(Arc::new).map_err(|err| {
                    TuliproxError::RegexCompile(format!("channel_groups.match_normalize {pattern} {err}"))
                })
            })
            .transpose()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelGroupsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub match_by: ChannelMatchBy,
    #[serde(default)]
    pub match_as_ascii: bool,
    #[serde(default, skip_serializing_if = "ChannelMatchNormalize::is_empty")]
    pub match_normalize: ChannelMatchNormalize,
    #[serde(default)]
    pub epg_conflict: EpgConflictMode,
    #[serde(default)]
    pub name_strip_quality: bool,
    #[serde(default = "default_min_members")]
    pub min_members: u16,
    #[serde(default)]
    pub policy_default: ChannelGroupPolicy,
    #[serde(default = "default_true")]
    pub allow_provider_grace: bool,
    #[serde(default)]
    pub selection: ChannelGroupSelectionConfig,
}

impl Default for ChannelGroupsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            match_by: ChannelMatchBy::default(),
            match_as_ascii: false,
            match_normalize: ChannelMatchNormalize::default(),
            epg_conflict: EpgConflictMode::default(),
            name_strip_quality: false,
            min_members: default_min_members(),
            policy_default: ChannelGroupPolicy::default(),
            allow_provider_grace: default_true(),
            selection: ChannelGroupSelectionConfig::default(),
        }
    }
}

impl ChannelGroupsConfig {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        if self.min_members < 2 {
            return Err(TuliproxError::ConfigTarget("`channel_groups.min_members` must be at least 2".to_string()));
        }
        if self.selection.max_start_attempts == 0 || self.selection.circuit_failures == 0 {
            return Err(TuliproxError::ConfigTarget(
                "`channel_groups.selection` attempts and failure thresholds must be > 0".to_string(),
            ));
        }
        if self.selection.input_circuit_failures < 2 {
            return Err(TuliproxError::ConfigTarget(
                "`channel_groups.selection.input_circuit_failures` must be at least 2".to_string(),
            ));
        }
        if self.selection.underrun_ratio_pct > 100 {
            return Err(TuliproxError::ConfigTarget(
                "`channel_groups.selection.underrun_ratio_pct` must be between 0 and 100".to_string(),
            ));
        }
        if self.selection.circuit_cooldown_max_secs < self.selection.circuit_cooldown_secs {
            self.selection.circuit_cooldown_max_secs = self.selection.circuit_cooldown_secs;
        }
        self.match_normalize.prepare()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let cfg: ChannelGroupsConfig = serde_json::from_str(r#"{"enabled":true}"#).expect("parses");
        assert!(cfg.enabled);
        assert_eq!(cfg.match_by, ChannelMatchBy::Caption);
        assert!(!cfg.match_as_ascii);
        assert_eq!(cfg.min_members, 2);
        assert_eq!(cfg.policy_default, ChannelGroupPolicy::Auto);
        assert!(cfg.allow_provider_grace);
        assert_eq!(cfg.selection, ChannelGroupSelectionConfig::default());
        assert_eq!(cfg.selection.circuit_failures, 2);
        assert_eq!(cfg.selection.circuit_cooldown_secs, 60);
        assert_eq!(cfg.selection.circuit_cooldown_max_secs, 900);
        assert_eq!(cfg.selection.startup_budget_secs, 15);
        assert_eq!(cfg.selection.max_start_attempts, 4);
        assert_eq!(cfg.selection.same_member_retries, 1);
        assert_eq!(cfg.selection.stall_timeout_secs, 8);
        assert_eq!(cfg.selection.runtime_switch_budget_secs, 20);
        assert_eq!(cfg.selection.input_circuit_failures, 3);
        assert_eq!(cfg.selection.input_failure_window_secs, 120);
        assert_eq!(cfg.selection.underrun_ratio_pct, 90);
        assert_eq!(cfg.selection.underrun_window_secs, 15);
        assert_eq!(cfg.selection.stable_reset_secs, 30);
        assert_eq!(cfg.selection.flap_window_secs, 600);
        assert_eq!(cfg.selection.flap_max_switches, 6);
        assert_eq!(cfg.match_normalize, ChannelMatchNormalize::default());
        assert_eq!(cfg.epg_conflict, EpgConflictMode::Split);
        assert!(!cfg.name_strip_quality);
        assert_eq!(cfg.selection.runtime_audio_language, RuntimeAudioLanguage::Prefer);
        assert!(!cfg.selection.early_start_response);
    }

    #[test]
    fn match_normalize_compiles_regex_on_prepare() {
        let mut cfg: ChannelGroupsConfig = serde_json::from_str(
            r#"{"enabled":true,"match_normalize":{"strip_regex":"^[A-Z]{2}:\\s*","ignore_tokens":["hevc"],"aliases":{"das erste":"ard"}}}"#,
        )
        .expect("parses");
        cfg.prepare().expect("valid");
        assert!(cfg.match_normalize.t_strip_regex.is_some());
        let mut bad: ChannelGroupsConfig =
            serde_json::from_str(r#"{"enabled":true,"match_normalize":{"strip_regex":"("}}"#).expect("parses");
        assert!(bad.prepare().is_err());
    }

    #[test]
    fn rejects_out_of_range_selection_values() {
        let mut ratio: ChannelGroupsConfig =
            serde_json::from_str(r#"{"enabled":true,"selection":{"underrun_ratio_pct":101}}"#).expect("parses");
        assert!(ratio.prepare().is_err());
        let mut input: ChannelGroupsConfig =
            serde_json::from_str(r#"{"enabled":true,"selection":{"input_circuit_failures":1}}"#).expect("parses");
        assert!(input.prepare().is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_min_members_below_two() {
        assert!(serde_json::from_str::<ChannelGroupsConfig>(r#"{"enabled":true,"weight_qos":5}"#).is_err());
        let mut cfg: ChannelGroupsConfig = serde_json::from_str(r#"{"enabled":true,"min_members":1}"#).expect("parses");
        assert!(cfg.prepare().is_err());
    }

    #[test]
    fn epg_channel_id_match_by_parses() {
        let cfg: ChannelGroupsConfig =
            serde_json::from_str(r#"{"enabled":true,"match_by":"epg_channel_id","policy_default":"priority"}"#)
                .expect("parses");
        assert_eq!(cfg.match_by, ChannelMatchBy::EpgChannelId);
        assert_eq!(cfg.policy_default, ChannelGroupPolicy::Priority);
    }
}
