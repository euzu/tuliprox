use super::{is_hls_provisioning_gap_segment, is_hls_provisioning_segment, SegmentCacheStatus, SegmentEntry};

const HLS_MASTER_FALLBACK_BANDWIDTH_BPS: u32 = 1_000_000;
const HLS_MASTER_BANDWIDTH_HEADROOM_PERCENT: u64 = 120;
const PERCENT_DENOMINATOR: u64 = 100;
const HLS_BANDWIDTH_MIN_READY_SEGMENTS: usize = 3;
pub const HLS_BANDWIDTH_PERSISTENCE_RETRY_MS: u64 = 30_000;

/// Measured stream bitrate and its conservative HLS master-playlist advertisement.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsMasterBandwidth {
    measured_bps: Option<u32>,
}

impl HlsMasterBandwidth {
    pub const fn new(measured_bps: Option<u32>) -> Self {
        let measured_bps = match measured_bps {
            Some(0) | None => None,
            Some(measured_bps) => Some(measured_bps),
        };
        Self { measured_bps }
    }

    pub const fn is_unknown(self) -> bool { self.measured_bps.is_none() }

    pub fn advertised_bps(self) -> u32 {
        let Some(measured_bps) = self.measured_bps else {
            return HLS_MASTER_FALLBACK_BANDWIDTH_BPS;
        };
        let advertised_bps = u64::from(measured_bps)
            .saturating_mul(HLS_MASTER_BANDWIDTH_HEADROOM_PERCENT)
            .saturating_add(PERCENT_DENOMINATOR.saturating_sub(1))
            / PERCENT_DENOMINATOR;
        u32::try_from(advertised_bps).unwrap_or(u32::MAX)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsMasterBandwidthSource {
    Item,
    Database,
    Fallback,
}

impl HlsMasterBandwidthSource {
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Database => "database",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsMasterBandwidthSelection {
    bandwidth: HlsMasterBandwidth,
    source: HlsMasterBandwidthSource,
}

impl HlsMasterBandwidthSelection {
    pub fn resolve(item_bitrate_bps: Option<u32>, database_bitrate_bps: Option<u32>) -> Self {
        let item = HlsMasterBandwidth::new(item_bitrate_bps);
        if !item.is_unknown() {
            return Self { bandwidth: item, source: HlsMasterBandwidthSource::Item };
        }
        let database = HlsMasterBandwidth::new(database_bitrate_bps);
        if !database.is_unknown() {
            return Self { bandwidth: database, source: HlsMasterBandwidthSource::Database };
        }
        Self { bandwidth: HlsMasterBandwidth::new(None), source: HlsMasterBandwidthSource::Fallback }
    }

    pub const fn bandwidth(self) -> HlsMasterBandwidth { self.bandwidth }

    pub const fn source(self) -> HlsMasterBandwidthSource { self.source }

    pub const fn known_bitrate_bps(self) -> Option<u32> { self.bandwidth.measured_bps }
}

/// Minimal single-variant HLS master playlist for one lease-bound media playlist.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsSingleVariantMasterPlaylist {
    bandwidth: HlsMasterBandwidth,
    media_playlist_uri: String,
}

impl HlsSingleVariantMasterPlaylist {
    pub fn new(bandwidth: HlsMasterBandwidth, media_playlist_uri: String) -> Self {
        Self { bandwidth, media_playlist_uri }
    }

    pub fn render(&self) -> String {
        format!(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH={}\n{}\n",
            self.bandwidth.advertised_bps(),
            self.media_playlist_uri
        )
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsBandwidthSample {
    proxy_seq: u64,
    duration_ms: u64,
    content_length: u64,
}

impl HlsBandwidthSample {
    pub fn from_segment(entry: &SegmentEntry) -> Option<Self> {
        let SegmentCacheStatus::Ready { content_length, .. } = &entry.status else {
            return None;
        };
        if entry.duration_ms == 0
            || *content_length == 0
            || is_hls_provisioning_segment(entry)
            || is_hls_provisioning_gap_segment(entry)
        {
            return None;
        }
        Some(Self { proxy_seq: entry.proxy_seq, duration_ms: entry.duration_ms, content_length: *content_length })
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum HlsBandwidthPersistenceState {
    #[default]
    Idle,
    InFlight {
        bitrate_bps: u32,
    },
    Persisted {
        bitrate_bps: u32,
    },
    PermanentlyInapplicable {
        bitrate_bps: u32,
    },
    RetryAfter {
        retry_at_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsBandwidthPersistenceOutcome {
    Persisted,
    RetryAfter,
    PermanentlyInapplicable,
}

pub fn estimate_hls_peak_bandwidth_bps(samples: &[HlsBandwidthSample], target_duration_ms: u64) -> Option<u32> {
    if target_duration_ms == 0 || eligible_sample_count(samples) < HLS_BANDWIDTH_MIN_READY_SEGMENTS {
        return None;
    }

    let mut peak_bps = None;
    for start in 0..samples.len() {
        let mut total_duration_ms = 0_u128;
        let mut total_content_length = 0_u128;
        let mut previous_proxy_seq: Option<u64> = None;

        for sample in &samples[start..] {
            if sample.duration_ms == 0 || sample.content_length == 0 {
                break;
            }
            if let Some(previous_proxy_seq) = previous_proxy_seq {
                let Some(expected_proxy_seq) = previous_proxy_seq.checked_add(1) else {
                    break;
                };
                if sample.proxy_seq != expected_proxy_seq {
                    break;
                }
            }
            previous_proxy_seq = Some(sample.proxy_seq);
            total_duration_ms = total_duration_ms.saturating_add(u128::from(sample.duration_ms));
            total_content_length = total_content_length.saturating_add(u128::from(sample.content_length));

            if duration_is_in_target_window(total_duration_ms, target_duration_ms) {
                peak_bps = Some(peak_bps.unwrap_or(0).max(bitrate_bps(total_content_length, total_duration_ms)));
            }
            if duration_exceeds_target_window(total_duration_ms, target_duration_ms) {
                break;
            }
        }
    }

    peak_bps.or_else(|| {
        samples
            .iter()
            .filter(|sample| sample.duration_ms > 0 && sample.content_length > 0)
            .map(|sample| bitrate_bps(u128::from(sample.content_length), u128::from(sample.duration_ms)))
            .max()
    })
}

fn eligible_sample_count(samples: &[HlsBandwidthSample]) -> usize {
    samples.iter().filter(|sample| sample.duration_ms > 0 && sample.content_length > 0).count()
}

fn duration_is_in_target_window(duration_ms: u128, target_duration_ms: u64) -> bool {
    let doubled_duration = duration_ms.saturating_mul(2);
    let target_duration_ms = u128::from(target_duration_ms);
    doubled_duration >= target_duration_ms && doubled_duration <= target_duration_ms.saturating_mul(3)
}

fn duration_exceeds_target_window(duration_ms: u128, target_duration_ms: u64) -> bool {
    duration_ms.saturating_mul(2) > u128::from(target_duration_ms).saturating_mul(3)
}

fn bitrate_bps(content_length: u128, duration_ms: u128) -> u32 {
    let numerator = content_length.saturating_mul(8).saturating_mul(1_000);
    let rounded = numerator.saturating_add(duration_ms.saturating_sub(1)) / duration_ms;
    u32::try_from(rounded).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        estimate_hls_peak_bandwidth_bps, HlsBandwidthSample, HlsMasterBandwidth, HlsMasterBandwidthSelection,
        HlsMasterBandwidthSource, HlsSingleVariantMasterPlaylist,
    };

    const fn sample(proxy_seq: u64, duration_ms: u64, content_length: u64) -> HlsBandwidthSample {
        HlsBandwidthSample { proxy_seq, duration_ms, content_length }
    }

    #[test]
    fn unknown_and_zero_bandwidth_use_exact_fallback_without_headroom() {
        for measured_bps in [None, Some(0)] {
            let bandwidth = HlsMasterBandwidth::new(measured_bps);
            assert!(bandwidth.is_unknown());
            assert_eq!(bandwidth.advertised_bps(), 1_000_000);
        }
    }

    #[test]
    fn known_bandwidth_applies_ceiling_headroom_and_saturates() {
        assert_eq!(HlsMasterBandwidth::new(Some(1)).advertised_bps(), 2);
        assert_eq!(HlsMasterBandwidth::new(Some(2_500_001)).advertised_bps(), 3_000_002);
        assert_eq!(HlsMasterBandwidth::new(Some(u32::MAX)).advertised_bps(), u32::MAX);
    }

    #[test]
    fn master_bandwidth_selection_reports_item_database_and_fallback_sources() {
        let item = HlsMasterBandwidthSelection::resolve(Some(3_000_000), Some(2_500_000));
        assert_eq!(item.source(), HlsMasterBandwidthSource::Item);
        assert_eq!(item.bandwidth().advertised_bps(), 3_600_000);

        let database = HlsMasterBandwidthSelection::resolve(None, Some(2_500_000));
        assert_eq!(database.source(), HlsMasterBandwidthSource::Database);
        assert_eq!(database.bandwidth().advertised_bps(), 3_000_000);

        let fallback = HlsMasterBandwidthSelection::resolve(Some(0), None);
        assert_eq!(fallback.source(), HlsMasterBandwidthSource::Fallback);
        assert_eq!(fallback.bandwidth().advertised_bps(), 1_000_000);
    }

    #[test]
    fn single_variant_master_playlist_renders_only_required_lines_and_trailing_newline() {
        let playlist = HlsSingleVariantMasterPlaylist::new(
            HlsMasterBandwidth::new(Some(2_500_000)),
            "/iptv/hls/shared/live/session/lease/manifest.m3u8".to_string(),
        );

        assert_eq!(
            playlist.render(),
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000\n/iptv/hls/shared/live/session/lease/manifest.m3u8\n"
        );
    }

    #[test]
    fn hls_runtime_bandwidth_requires_three_eligible_samples() {
        assert_eq!(
            estimate_hls_peak_bandwidth_bps(&[sample(1, 2_000, 250_000), sample(2, 2_000, 250_000)], 4_000),
            None
        );
        assert_eq!(
            estimate_hls_peak_bandwidth_bps(
                &[sample(1, 2_000, 250_000), sample(2, 0, 250_000), sample(3, 2_000, 250_000)],
                4_000,
            ),
            None
        );
    }

    #[test]
    fn hls_runtime_bandwidth_uses_highest_contiguous_target_window() {
        let samples = [sample(10, 2_000, 250_000), sample(11, 2_000, 500_000), sample(12, 2_000, 250_000)];

        assert_eq!(estimate_hls_peak_bandwidth_bps(&samples, 4_000), Some(2_000_000));
    }

    #[test]
    fn hls_runtime_bandwidth_never_mixes_non_contiguous_sequences() {
        let samples = [sample(1, 1_000, 1_000_000), sample(3, 1_000, 100), sample(4, 1_000, 100)];

        assert_eq!(estimate_hls_peak_bandwidth_bps(&samples, 4_000), Some(800));
    }

    #[test]
    fn hls_runtime_bandwidth_falls_back_to_highest_individual_sample() {
        let samples = [sample(1, 1_000, 100_000), sample(2, 1_000, 300_000), sample(3, 1_000, 200_000)];

        assert_eq!(estimate_hls_peak_bandwidth_bps(&samples, 10_000), Some(2_400_000));
    }

    #[test]
    fn hls_runtime_bandwidth_ceiling_and_overflow_are_safe() {
        let samples = [sample(1, 1_000, 1), sample(2, 1_000, u64::MAX), sample(3, 1_000, 1)];

        assert_eq!(estimate_hls_peak_bandwidth_bps(&samples, 2_000), Some(u32::MAX));
        assert_eq!(
            estimate_hls_peak_bandwidth_bps(&[sample(1, 3_000, 1), sample(2, 3_000, 1), sample(3, 3_000, 1)], 4_000,),
            Some(3)
        );
    }
}
