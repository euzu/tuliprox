use super::HlsResourceFetchKind;
use arc_swap::ArcSwap;
use shared::model::{HlsSegmentTimeout, Millis};
use std::sync::Arc;

/// Updated atomically; worker clones retain this handle without replacing their slots.
#[derive(Debug, Clone)]
pub struct HlsOriginTimeoutPolicy(Arc<ArcSwap<HlsOriginTimeoutConfig>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsOriginTimeoutConfig {
    pub segment: HlsSegmentTimeout,
    pub manifest_ms: u64,
    pub connect_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlsOriginTimeoutSnapshot {
    pub kind: HlsResourceFetchKind,
    pub mode: HlsSegmentTimeout,
    pub session_target_duration: Option<u32>,
    pub effective_ms: u64,
}

impl HlsOriginTimeoutPolicy {
    pub fn new(config: HlsOriginTimeoutConfig) -> Self { Self(Arc::new(ArcSwap::from_pointee(config))) }
    pub fn fixed(segment_ms: u64, manifest_ms: u64) -> Self {
        Self::new(HlsOriginTimeoutConfig {
            segment: HlsSegmentTimeout::Fixed(Millis::new(segment_ms)),
            manifest_ms,
            ..HlsOriginTimeoutConfig::default()
        })
    }
    pub fn config(&self) -> HlsOriginTimeoutConfig { **self.0.load() }
    pub fn update(&self, config: HlsOriginTimeoutConfig) { self.0.store(Arc::new(config)); }
    pub fn snapshot(&self, kind: HlsResourceFetchKind, target_duration: Option<u32>) -> HlsOriginTimeoutSnapshot {
        self.config().snapshot(kind, target_duration)
    }
}

impl HlsOriginTimeoutConfig {
    pub fn snapshot(self, kind: HlsResourceFetchKind, target_duration: Option<u32>) -> HlsOriginTimeoutSnapshot {
        let mode = match kind {
            HlsResourceFetchKind::Key | HlsResourceFetchKind::Map => {
                HlsSegmentTimeout::Fixed(Millis::new(self.manifest_ms))
            }
            _ => self.segment,
        };
        let effective_ms = match mode {
            HlsSegmentTimeout::Auto => {
                target_duration.filter(|value| *value > 0).map_or(10_000, |value| u64::from(value) * 1_000)
            }
            HlsSegmentTimeout::Fixed(value) => value.get(),
        }
        .max(1);
        HlsOriginTimeoutSnapshot { kind, mode, session_target_duration: target_duration, effective_ms }
    }
}

impl Default for HlsOriginTimeoutConfig {
    fn default() -> Self {
        Self {
            segment: shared::defaults::default_hls_origin_segment_timeout_ms(),
            manifest_ms: shared::defaults::default_hls_origin_manifest_timeout_ms().get(),
            connect_ms: u64::from(shared::defaults::default_connect_timeout_secs()) * 1_000,
        }
    }
}

impl Default for HlsOriginTimeoutPolicy {
    fn default() -> Self { Self::new(HlsOriginTimeoutConfig::default()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HlsResourceFetchKind::{Key, Map, Other, Part, Segment};

    #[test]
    fn auto_uses_shared_duration_and_resources_use_their_own_budgets() {
        let policy = HlsOriginTimeoutPolicy::new(HlsOriginTimeoutConfig {
            segment: HlsSegmentTimeout::Auto,
            manifest_ms: 8_000,
            connect_ms: 6_000,
        });
        for (duration, expected) in [
            (Some(2), 2_000),
            (Some(4), 4_000),
            (Some(12), 12_000),
            (None, 10_000),
            (Some(0), 10_000),
            (Some(u32::MAX), u64::from(u32::MAX) * 1_000),
        ] {
            for kind in [Segment, Part, Other] {
                assert_eq!(policy.snapshot(kind, duration).effective_ms, expected);
            }
            for kind in [Key, Map] {
                assert_eq!(policy.snapshot(kind, duration).effective_ms, 8_000);
            }
        }
        let frozen = policy.snapshot(Segment, Some(2));
        policy.update(HlsOriginTimeoutConfig {
            segment: HlsSegmentTimeout::Fixed(Millis::new(700)),
            manifest_ms: 900,
            connect_ms: 6_000,
        });
        assert_eq!(frozen.effective_ms, 2_000);
        assert_eq!(policy.snapshot(Segment, Some(12)).effective_ms, 700);
        assert_eq!(policy.snapshot(Map, Some(12)).effective_ms, 900);
    }
}
