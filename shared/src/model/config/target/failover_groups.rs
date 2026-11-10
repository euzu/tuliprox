use crate::error::TuliproxError;
use regex::Regex;
use std::{collections::BTreeMap, sync::Arc};

/// A unit-variant config enum with its YAML names, `ALL`, `Display` and `FromStr`.
macro_rules! config_enum {
    ($(#[$meta:meta])* $name:ident, $key:literal { $default:ident => $default_str:literal $(, $variant:ident => $str:literal)* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub enum $name {
            #[default]
            #[serde(rename = $default_str)]
            $default,
            $(#[serde(rename = $str)] $variant,)*
        }

        impl $name {
            pub const ALL: &'static [Self] = &[Self::$default $(, Self::$variant)*];

            /// The name used in the configuration.
            pub const fn as_str(self) -> &'static str {
                match self {
                    Self::$default => $default_str,
                    $(Self::$variant => $str,)*
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }

        impl std::str::FromStr for $name {
            type Err = TuliproxError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::ALL
                    .iter()
                    .copied()
                    .find(|variant| variant.as_str() == value)
                    .ok_or_else(|| TuliproxError::ConfigBase(format!("unknown failover_groups.{}: {value}", $key)))
            }
        }
    };
}

config_enum!(ChannelMatchBy, "match_by" {
    Caption => "caption",
    Name => "name",
    Title => "title",
    EpgChannelId => "epg_channel_id",
});

config_enum!(
    /// Member order of a group: `auto` ranks by health and quality, `priority` keeps the list order.
    FailoverGroupPolicy, "policy" { Auto => "auto", Priority => "priority" }
);

config_enum!(RuntimeAudioLanguage, "failover.audio_language" { Prefer => "prefer", Require => "require" });

config_enum!(EpgConflictMode, "epg_conflict" { Split => "split", Ignore => "ignore" });

/// Grouping-only match key extensions; `deduplicate` never uses them.
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
                    TuliproxError::RegexCompile(format!("failover_groups.match_normalize {pattern} {err}"))
                })
            })
            .transpose()?;
        Ok(())
    }
}

/// A group always has at least this many members; a single channel stays a normal channel.
pub const MIN_GROUP_MEMBERS: usize = 2;

const fn default_true() -> bool { true }

/// How a client start picks a member.
#[derive(Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FailoverGroupStartConfig {
    /// Longest time a start may try members; 0 means no limit.
    pub timeout_secs: u64,
    /// Most members tried at start.
    pub max_attempts: u8,
}

impl Default for FailoverGroupStartConfig {
    fn default() -> Self { Self { timeout_secs: 15, max_attempts: 4 } }
}

/// Switching to another member while a client watches an MPEG-TS stream.
#[derive(Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FailoverGroupRuntimeConfig {
    pub enabled: bool,
    /// Seconds without media before the member is replaced; 0 disables the check.
    pub no_data_timeout_secs: u64,
    /// A member delivering less than this percentage of real-time speed is too slow; 0 disables it.
    pub slow_source_pct: u8,
    /// How long a member must stay too slow before it is replaced.
    pub slow_source_secs: u64,
    /// Longest time without playback while looking for a replacement.
    pub max_outage_secs: u64,
    /// Retries of a member that played stably before others are tried.
    pub same_source_retries: u8,
    /// Playback after which a member counts as stable and an incident is over.
    pub stable_after_secs: u64,
    /// Most switches within `max_switches_window_secs`; 0 disables the limit.
    pub max_switches: u8,
    pub max_switches_window_secs: u64,
    pub audio_language: RuntimeAudioLanguage,
}

impl Default for FailoverGroupRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            no_data_timeout_secs: 8,
            slow_source_pct: 90,
            slow_source_secs: 15,
            max_outage_secs: 20,
            same_source_retries: 1,
            stable_after_secs: 30,
            max_switches: 6,
            max_switches_window_secs: 600,
            audio_language: RuntimeAudioLanguage::Prefer,
        }
    }
}

/// When failing members and inputs are skipped for a while.
#[derive(Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FailoverGroupCircuitConfig {
    /// Consecutive failures that put a member on hold.
    pub source_failures: u8,
    /// First hold time; doubles while the member keeps failing.
    pub cooldown_secs: u64,
    pub cooldown_max_secs: u64,
    /// Failures of one input (from at least two members) within `input_window_secs` that put
    /// the whole input back.
    pub input_failures: u8,
    pub input_window_secs: u64,
}

impl Default for FailoverGroupCircuitConfig {
    fn default() -> Self {
        Self {
            source_failures: 2,
            cooldown_secs: 60,
            cooldown_max_secs: 900,
            input_failures: 3,
            input_window_secs: 120,
        }
    }
}

/// `config.yml` `failover_groups`: the settings every target with failover groups uses.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FailoverGroupsConfig {
    pub match_by: ChannelMatchBy,
    pub match_as_ascii: bool,
    #[serde(skip_serializing_if = "ChannelMatchNormalize::is_empty")]
    pub match_normalize: ChannelMatchNormalize,
    pub epg_conflict: EpgConflictMode,
    pub name_strip_quality: bool,
    pub policy: FailoverGroupPolicy,
    #[serde(default = "default_true")]
    pub allow_provider_grace: bool,
    pub start: FailoverGroupStartConfig,
    pub failover: FailoverGroupRuntimeConfig,
    pub circuit: FailoverGroupCircuitConfig,
}

impl Default for FailoverGroupsConfig {
    fn default() -> Self {
        Self {
            match_by: ChannelMatchBy::default(),
            match_as_ascii: false,
            match_normalize: ChannelMatchNormalize::default(),
            epg_conflict: EpgConflictMode::default(),
            name_strip_quality: false,
            policy: FailoverGroupPolicy::default(),
            allow_provider_grace: true,
            start: FailoverGroupStartConfig::default(),
            failover: FailoverGroupRuntimeConfig::default(),
            circuit: FailoverGroupCircuitConfig::default(),
        }
    }
}

/// The settings a group playback of one target uses.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct FailoverGroupPlayback {
    pub start: FailoverGroupStartConfig,
    pub failover: FailoverGroupRuntimeConfig,
    pub allow_provider_grace: bool,
}

impl FailoverGroupsConfig {
    pub fn is_default(&self) -> bool { *self == Self::default() }

    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        let invalid = |msg: &str| Err(TuliproxError::ConfigBase(format!("`failover_groups.{msg}")));
        if self.start.max_attempts == 0 {
            return invalid("start.max_attempts` must be > 0");
        }
        if self.circuit.source_failures == 0 {
            return invalid("circuit.source_failures` must be > 0");
        }
        if self.circuit.input_failures < 2 {
            return invalid("circuit.input_failures` must be at least 2");
        }
        if self.failover.slow_source_pct > 100 {
            return invalid("failover.slow_source_pct` must be between 0 and 100");
        }
        if self.failover.max_outage_secs == 0 {
            return invalid("failover.max_outage_secs` must be > 0");
        }
        if self.circuit.cooldown_max_secs < self.circuit.cooldown_secs {
            self.circuit.cooldown_max_secs = self.circuit.cooldown_secs;
        }
        self.match_normalize.prepare()
    }

    /// The grouping settings of `target`: these settings with the target's overrides applied.
    #[must_use]
    pub fn for_target(&self, target: &FailoverGroupsTargetConfig) -> Self {
        let mut config = self.clone();
        config.match_by = target.match_by.unwrap_or(config.match_by);
        config.match_as_ascii = target.match_as_ascii.unwrap_or(config.match_as_ascii);
        config.epg_conflict = target.epg_conflict.unwrap_or(config.epg_conflict);
        config.name_strip_quality = target.name_strip_quality.unwrap_or(config.name_strip_quality);
        config.policy = target.policy.unwrap_or(config.policy);
        config.allow_provider_grace = target.allow_provider_grace.unwrap_or(config.allow_provider_grace);
        config.failover.enabled = target.failover.unwrap_or(config.failover.enabled);
        config
    }

    /// The playback settings of a target, without copying the grouping settings.
    pub fn playback(&self, target: Option<&FailoverGroupsTargetConfig>) -> FailoverGroupPlayback {
        let mut failover = self.failover;
        failover.enabled = target.and_then(|target| target.failover).unwrap_or(failover.enabled);
        FailoverGroupPlayback {
            start: self.start,
            failover,
            allow_provider_grace: target
                .and_then(|target| target.allow_provider_grace)
                .unwrap_or(self.allow_provider_grace),
        }
    }
}

/// `source.yml` `targets[].options.failover_groups`: turns failover groups on for a target and
/// overrides the global settings where a target needs it. Unset fields use `config.yml`.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FailoverGroupsTargetConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_by: Option<ChannelMatchBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_as_ascii: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epg_conflict: Option<EpgConflictMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_strip_quality: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<FailoverGroupPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_provider_grace: Option<bool>,
    /// Switching members while watching; `None` uses `failover.enabled` of `config.yml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failover: Option<bool>,
}

impl FailoverGroupsTargetConfig {
    pub fn is_default(&self) -> bool { *self == Self::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_by_round_trips_through_its_config_name() {
        for &match_by in ChannelMatchBy::ALL {
            let name = match_by.to_string();
            assert_eq!(name.parse::<ChannelMatchBy>().expect("known name"), match_by);
            assert_eq!(serde_json::to_string(&match_by).expect("serializes"), format!("\"{name}\""));
        }
        assert!("unknown".parse::<ChannelMatchBy>().is_err());
    }

    #[test]
    fn defaults_match_design() {
        let cfg: FailoverGroupsConfig = serde_json::from_str("{}").expect("parses");
        assert_eq!(cfg, FailoverGroupsConfig::default());
        assert_eq!(cfg.match_by, ChannelMatchBy::Caption);
        assert_eq!(cfg.policy, FailoverGroupPolicy::Auto);
        assert!(cfg.allow_provider_grace);
        assert_eq!(cfg.start, FailoverGroupStartConfig { timeout_secs: 15, max_attempts: 4 });
        assert!(cfg.failover.enabled);
        assert_eq!(cfg.failover.no_data_timeout_secs, 8);
        assert_eq!(cfg.failover.slow_source_pct, 90);
        assert_eq!(cfg.failover.slow_source_secs, 15);
        assert_eq!(cfg.failover.max_outage_secs, 20);
        assert_eq!(cfg.failover.same_source_retries, 1);
        assert_eq!(cfg.failover.stable_after_secs, 30);
        assert_eq!(cfg.failover.max_switches, 6);
        assert_eq!(cfg.failover.max_switches_window_secs, 600);
        assert_eq!(cfg.failover.audio_language, RuntimeAudioLanguage::Prefer);
        assert_eq!(
            cfg.circuit,
            FailoverGroupCircuitConfig {
                source_failures: 2,
                cooldown_secs: 60,
                cooldown_max_secs: 900,
                input_failures: 3,
                input_window_secs: 120
            }
        );
        assert_eq!(cfg.epg_conflict, EpgConflictMode::Split);
        assert!(!cfg.name_strip_quality);
    }

    #[test]
    fn match_normalize_compiles_regex_on_prepare() {
        let mut cfg: FailoverGroupsConfig = serde_json::from_str(
            r#"{"match_normalize":{"strip_regex":"^[A-Z]{2}:\\s*","ignore_tokens":["hevc"],"aliases":{"das erste":"ard"}}}"#,
        )
        .expect("parses");
        cfg.prepare().expect("valid");
        assert!(cfg.match_normalize.t_strip_regex.is_some());
        let mut bad: FailoverGroupsConfig =
            serde_json::from_str(r#"{"match_normalize":{"strip_regex":"("}}"#).expect("parses");
        assert!(bad.prepare().is_err());
    }

    #[test]
    fn rejects_out_of_range_values() {
        for json in [
            r#"{"failover":{"slow_source_pct":101}}"#,
            r#"{"failover":{"max_outage_secs":0}}"#,
            r#"{"circuit":{"input_failures":1}}"#,
            r#"{"circuit":{"source_failures":0}}"#,
            r#"{"start":{"max_attempts":0}}"#,
        ] {
            let mut cfg: FailoverGroupsConfig = serde_json::from_str(json).expect("parses");
            assert!(cfg.prepare().is_err(), "{json}");
        }
    }

    #[test]
    fn rejects_unknown_and_removed_fields() {
        assert!(serde_json::from_str::<FailoverGroupsConfig>(r#"{"min_members":2}"#).is_err());
        assert!(serde_json::from_str::<FailoverGroupsConfig>(r#"{"selection":{}}"#).is_err());
        assert!(serde_json::from_str::<FailoverGroupsTargetConfig>(r#"{"enabled":true,"min_members":2}"#).is_err());
        assert!(serde_json::from_str::<FailoverGroupsTargetConfig>(r#"{"enabled":true,"match_normalize":{}}"#).is_err());
    }

    #[test]
    fn cooldown_max_is_raised_to_cooldown() {
        let mut cfg: FailoverGroupsConfig =
            serde_json::from_str(r#"{"circuit":{"cooldown_secs":120,"cooldown_max_secs":60}}"#).expect("parses");
        cfg.prepare().expect("valid");
        assert_eq!(cfg.circuit.cooldown_max_secs, 120);
    }

    #[test]
    fn target_overrides_replace_only_set_fields() {
        let global = FailoverGroupsConfig {
            match_by: ChannelMatchBy::Name,
            name_strip_quality: true,
            failover: FailoverGroupRuntimeConfig { no_data_timeout_secs: 12, ..FailoverGroupRuntimeConfig::default() },
            ..FailoverGroupsConfig::default()
        };
        let unset = FailoverGroupsTargetConfig { enabled: true, ..FailoverGroupsTargetConfig::default() };
        assert_eq!(global.for_target(&unset), global);

        let target: FailoverGroupsTargetConfig = serde_json::from_str(
            r#"{"enabled":true,"match_by":"epg_channel_id","name_strip_quality":false,"policy":"priority","allow_provider_grace":false,"failover":false}"#,
        )
        .expect("parses");
        let effective = global.for_target(&target);
        assert_eq!(effective.match_by, ChannelMatchBy::EpgChannelId);
        assert!(!effective.name_strip_quality);
        assert_eq!(effective.policy, FailoverGroupPolicy::Priority);
        assert!(!effective.allow_provider_grace);
        assert!(!effective.failover.enabled);
        assert_eq!(effective.failover.no_data_timeout_secs, 12);
        assert_eq!(effective.match_as_ascii, global.match_as_ascii);
    }

    #[test]
    fn playback_applies_target_failover_and_grace() {
        let global = FailoverGroupsConfig::default();
        let target = FailoverGroupsTargetConfig {
            enabled: true,
            allow_provider_grace: Some(false),
            failover: Some(false),
            ..FailoverGroupsTargetConfig::default()
        };
        let playback = global.playback(Some(&target));
        assert!(!playback.allow_provider_grace);
        assert!(!playback.failover.enabled);
        assert_eq!(playback.start, global.start);
        let defaults = global.playback(None);
        assert!(defaults.allow_provider_grace);
        assert!(defaults.failover.enabled);
    }

    #[test]
    fn target_serializes_only_overrides() {
        let target = FailoverGroupsTargetConfig { enabled: true, failover: Some(false), ..Default::default() };
        assert_eq!(serde_json::to_string(&target).expect("serializes"), r#"{"enabled":true,"failover":false}"#);
    }
}
