use crate::{
    error::TuliproxError,
    utils::{
        default_as_true, default_catchup_session_ttl_secs, default_grace_period_millis,
        default_grace_period_timeout_secs, default_hls_session_ttl_secs, default_shared_burst_buffer_mb,
        is_blank_optional_string, is_default_catchup_session_ttl_secs, is_default_grace_period_millis,
        is_default_grace_period_timeout_secs, is_default_hls_session_ttl_secs, is_default_shared_burst_buffer_mb,
        is_true, parse_to_kbps,
    },
};
use std::{
    fmt::{Display, Formatter},
    str::FromStr,
};

const STREAM_QUEUE_SIZE: usize = 1024; // mpsc channel holding messages. with 8192byte chunks and 2Mbit/s approx 8MB
const MIN_SHARED_BURST_BUFFER_MB: u64 = 1;

#[derive(Debug, Copy, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub enum AdmissionStrategy {
    #[serde(rename = "evict_user_same_ip_oldest")]
    EvictUserSameIpOldest,
    #[serde(rename = "evict_user_same_ip_latest")]
    EvictUserSameIpLatest,
    #[serde(rename = "evict_user_oldest")]
    EvictUserOldest,
    #[serde(rename = "evict_user_latest")]
    EvictUserLatest,
    #[serde(rename = "grace_instant_stream")]
    GraceInstantStream,
    #[serde(rename = "grace_hold_stream")]
    GraceHoldStream,
}

impl AdmissionStrategy {
    pub fn is_grace(&self) -> bool { matches!(self, Self::GraceInstantStream | Self::GraceHoldStream) }

    pub fn is_grace_hold(&self) -> bool { matches!(self, Self::GraceHoldStream) }
}

impl Display for AdmissionStrategy {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                AdmissionStrategy::EvictUserSameIpOldest => "evict_user_same_ip_oldest",
                AdmissionStrategy::EvictUserSameIpLatest => "evict_user_same_ip_latest",
                AdmissionStrategy::EvictUserOldest => "evict_user_oldest",
                AdmissionStrategy::EvictUserLatest => "evict_user_latest",
                AdmissionStrategy::GraceInstantStream => "grace_instant_stream",
                AdmissionStrategy::GraceHoldStream => "grace_hold_stream",
            }
        )
    }
}

impl FromStr for AdmissionStrategy {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "evict_user_same_ip_oldest" => Ok(AdmissionStrategy::EvictUserSameIpOldest),
            "evict_user_same_ip_latest" => Ok(AdmissionStrategy::EvictUserSameIpLatest),
            "evict_user_oldest" => Ok(AdmissionStrategy::EvictUserOldest),
            "evict_user_latest" => Ok(AdmissionStrategy::EvictUserLatest),
            "grace_instant_stream" => Ok(AdmissionStrategy::GraceInstantStream),
            "grace_hold_stream" => Ok(AdmissionStrategy::GraceHoldStream),
            _ => Err(TuliproxError::Config(format!("Unknown admission strategy: {s}"))),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StreamBufferConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub size: usize,
}

impl StreamBufferConfigDto {
    pub fn is_empty(&self) -> bool { !self.enabled && self.size == 0 }
    fn prepare(&mut self) {
        if self.enabled && self.size == 0 {
            self.size = STREAM_QUEUE_SIZE;
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StreamConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub retry: bool,
    #[serde(default, skip_serializing_if = "crate::utils::is_false")]
    pub metrics_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer: Option<StreamBufferConfigDto>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub throttle: Option<String>,
    #[serde(default = "default_grace_period_millis", skip_serializing_if = "is_default_grace_period_millis")]
    pub grace_period_millis: u64,
    #[serde(
        default = "default_grace_period_timeout_secs",
        skip_serializing_if = "is_default_grace_period_timeout_secs"
    )]
    pub grace_period_timeout_secs: u64,
    /// If true (default), wait for a grace period check before streaming.
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub grace_period_hold_stream: bool,
    #[serde(default = "default_hls_session_ttl_secs", skip_serializing_if = "is_default_hls_session_ttl_secs")]
    pub hls_session_ttl_secs: u64,
    #[serde(default = "default_catchup_session_ttl_secs", skip_serializing_if = "is_default_catchup_session_ttl_secs")]
    pub catchup_session_ttl_secs: u64,
    #[serde(default, skip)]
    pub throttle_kbps: u64,
    #[serde(default = "default_shared_burst_buffer_mb", skip_serializing_if = "is_default_shared_burst_buffer_mb")]
    pub shared_burst_buffer_mb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_strategies: Option<Vec<AdmissionStrategy>>,
}

impl Default for StreamConfigDto {
    fn default() -> Self {
        StreamConfigDto {
            retry: true,
            metrics_enabled: false,
            buffer: None,
            throttle: None,
            grace_period_millis: default_grace_period_millis(),
            grace_period_timeout_secs: default_grace_period_timeout_secs(),
            throttle_kbps: 0,
            shared_burst_buffer_mb: default_shared_burst_buffer_mb(),
            grace_period_hold_stream: true,
            hls_session_ttl_secs: default_hls_session_ttl_secs(),
            catchup_session_ttl_secs: default_catchup_session_ttl_secs(),
            admission_strategies: None,
        }
    }
}

impl StreamConfigDto {
    pub fn is_empty(&self) -> bool {
        self.retry
            && !self.metrics_enabled
            && (self.buffer.is_none() || self.buffer.as_ref().is_some_and(|b| b.is_empty()))
            && (self.throttle.is_none() || self.throttle.as_ref().is_some_and(|t| t.is_empty()))
            && self.grace_period_millis == default_grace_period_millis()
            && self.grace_period_timeout_secs == default_grace_period_timeout_secs()
            && self.throttle_kbps == 0
            && self.shared_burst_buffer_mb == default_shared_burst_buffer_mb()
            && self.grace_period_hold_stream
            && self.hls_session_ttl_secs == default_hls_session_ttl_secs()
            && self.catchup_session_ttl_secs == default_catchup_session_ttl_secs()
            && self.admission_strategies.is_none()
    }

    pub(crate) fn prepare(&mut self) -> Result<(), TuliproxError> {
        if let Some(buffer) = self.buffer.as_mut() {
            buffer.prepare();
        }
        if let Some(throttle) = &self.throttle {
            parse_to_kbps(throttle).map_err(TuliproxError::ConfigStream)?;
        } else {
            self.throttle_kbps = 0;
        }

        if self.grace_period_millis > 0 {
            if self.grace_period_timeout_secs == 0 {
                let triple_ms = self.grace_period_millis.saturating_mul(3);
                self.grace_period_timeout_secs = std::cmp::max(1, triple_ms.div_ceil(1000));
            } else if self.grace_period_millis / 1000 > self.grace_period_timeout_secs {
                return Err(TuliproxError::ConfigStream(format!(
                    "Grace time period timeout {} sec should be more than grace time period {} ms",
                    self.grace_period_timeout_secs, self.grace_period_millis
                )));
            }
        }

        if self.shared_burst_buffer_mb < MIN_SHARED_BURST_BUFFER_MB {
            return Err(TuliproxError::ConfigStream(format!(
                "`shared_burst_buffer_mb` must be at least {MIN_SHARED_BURST_BUFFER_MB} MB"
            )));
        }

        if let Some(strategies) = &self.admission_strategies {
            if let Err(err) = validate_admission_strategies(strategies, self.grace_period_millis) {
                return Err(TuliproxError::ConfigStream(err));
            }
        }

        Ok(())
    }
}

fn validate_admission_strategies(strategies: &[AdmissionStrategy], grace_period_millis: u64) -> Result<(), String> {
    use std::collections::HashSet;
    use AdmissionStrategy::*;

    let mut seen = HashSet::new();
    for s in strategies {
        if !seen.insert(*s) {
            return Err(format!(
                "Duplicate admission strategy: {}",
                serde_json::to_string(s).unwrap_or_default().trim_matches('"')
            ));
        }
    }

    let has_instant = strategies.iter().any(|s| matches!(s, GraceInstantStream));
    let has_hold = strategies.iter().any(|s| matches!(s, GraceHoldStream));
    if has_instant && has_hold {
        return Err("admission_strategies: grace_instant_stream and grace_hold_stream are mutually exclusive".into());
    }

    if grace_period_millis == 0 && (has_instant || has_hold) {
        return Err("admission_strategies: grace strategies require grace_period_millis > 0".into());
    }

    validate_admission_strategy_order(strategies)?;

    Ok(())
}

pub fn is_valid_admission_strategy_order(strategies: &[AdmissionStrategy]) -> bool {
    validate_admission_strategy_order(strategies).is_ok()
}

pub fn validate_admission_strategy_order(strategies: &[AdmissionStrategy]) -> Result<(), String> {
    use AdmissionStrategy::*;

    fn strategy_index(strategies: &[AdmissionStrategy], strategy: AdmissionStrategy) -> Option<usize> {
        strategies.iter().position(|candidate| *candidate == strategy)
    }

    for broader in [EvictUserOldest, EvictUserLatest] {
        if let Some(broader_idx) = strategy_index(strategies, broader) {
            for narrower in [EvictUserSameIpOldest, EvictUserSameIpLatest] {
                if let Some(narrower_idx) = strategy_index(strategies, narrower) {
                    if broader_idx < narrower_idx {
                        let broader_name =
                            serde_json::to_string(&broader).unwrap_or_default().trim_matches('"').to_string();
                        let narrower_name =
                            serde_json::to_string(&narrower).unwrap_or_default().trim_matches('"').to_string();
                        return Err(format!(
                            "admission_strategies: {broader_name} must not appear before {narrower_name} because the later rule would be shadowed"
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_list_accepted() {
        let mut dto = StreamConfigDto { admission_strategies: Some(vec![]), ..StreamConfigDto::default() };
        assert!(dto.prepare().is_ok());
    }

    #[test]
    fn test_none_strategies_accepted() {
        let mut dto = StreamConfigDto::default();
        assert!(dto.prepare().is_ok());
    }

    #[test]
    fn test_duplicate_rejected() {
        let mut dto = StreamConfigDto {
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserSameIpOldest,
                AdmissionStrategy::EvictUserSameIpOldest,
            ]),
            ..StreamConfigDto::default()
        };
        let err = dto.prepare().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Duplicate admission strategy"), "msg: {msg}");
    }

    #[test]
    fn test_mutually_exclusive_grace_rejected() {
        let mut dto = StreamConfigDto {
            admission_strategies: Some(vec![AdmissionStrategy::GraceInstantStream, AdmissionStrategy::GraceHoldStream]),
            ..StreamConfigDto::default()
        };
        let err = dto.prepare().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("mutually exclusive"), "msg: {msg}");
    }

    #[test]
    fn test_valid_ordered_subset_accepted() {
        let mut dto = StreamConfigDto {
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest, AdmissionStrategy::GraceHoldStream]),
            ..StreamConfigDto::default()
        };
        assert!(dto.prepare().is_ok());
    }

    #[test]
    fn test_safe_ordering_same_ip_oldest_before_oldest_accepted() {
        let mut dto = StreamConfigDto {
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserSameIpOldest,
                AdmissionStrategy::EvictUserOldest,
            ]),
            ..StreamConfigDto::default()
        };
        assert!(dto.prepare().is_ok());
    }

    #[test]
    fn test_safe_ordering_same_ip_latest_before_latest_accepted() {
        let mut dto = StreamConfigDto {
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserSameIpLatest,
                AdmissionStrategy::EvictUserLatest,
            ]),
            ..StreamConfigDto::default()
        };
        assert!(dto.prepare().is_ok());
    }

    #[test]
    fn test_grace_strategy_requires_positive_grace_period() {
        let mut dto = StreamConfigDto {
            grace_period_millis: 0,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
            ..StreamConfigDto::default()
        };
        let err = dto.prepare().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("grace_period_millis"), "msg: {msg}");
    }

    #[test]
    fn test_shadowed_same_ip_oldest_order_rejected() {
        let mut dto = StreamConfigDto {
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserOldest,
                AdmissionStrategy::EvictUserSameIpOldest,
            ]),
            ..StreamConfigDto::default()
        };
        let err = dto.prepare().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("shadowed"), "msg: {msg}");
    }

    #[test]
    fn test_shadowed_same_ip_latest_order_rejected() {
        let mut dto = StreamConfigDto {
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserLatest,
                AdmissionStrategy::EvictUserSameIpLatest,
            ]),
            ..StreamConfigDto::default()
        };
        let err = dto.prepare().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("shadowed"), "msg: {msg}");
    }

    #[test]
    fn test_cross_order_pair_with_different_selection_policy_is_rejected() {
        let mut dto = StreamConfigDto {
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserOldest,
                AdmissionStrategy::EvictUserSameIpLatest,
            ]),
            ..StreamConfigDto::default()
        };
        let err = dto.prepare().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("shadowed"), "msg: {msg}");
    }

    #[test]
    fn test_single_strategy_accepted() {
        for s in [
            AdmissionStrategy::EvictUserSameIpOldest,
            AdmissionStrategy::EvictUserSameIpLatest,
            AdmissionStrategy::EvictUserOldest,
            AdmissionStrategy::EvictUserLatest,
            AdmissionStrategy::GraceInstantStream,
            AdmissionStrategy::GraceHoldStream,
        ] {
            let mut dto = StreamConfigDto { admission_strategies: Some(vec![s]), ..StreamConfigDto::default() };
            assert!(dto.prepare().is_ok(), "Failed for {s:?}");
        }
    }

    #[test]
    fn test_is_empty_with_strategies() {
        let dto = StreamConfigDto {
            admission_strategies: Some(vec![AdmissionStrategy::GraceInstantStream]),
            ..StreamConfigDto::default()
        };

        assert!(!dto.is_empty());
    }

    #[test]
    fn test_is_empty_with_explicit_empty_strategy_list() {
        let dto = StreamConfigDto { admission_strategies: Some(vec![]), ..StreamConfigDto::default() };
        assert!(!dto.is_empty());
    }

    #[test]
    fn test_explicit_empty_strategy_list_serializes() {
        let dto = StreamConfigDto { admission_strategies: Some(vec![]), ..StreamConfigDto::default() };

        let serialized = serde_json::to_string(&dto).unwrap_or_default();
        assert!(serialized.contains("admission_strategies"), "serialized: {serialized}");
    }
}
