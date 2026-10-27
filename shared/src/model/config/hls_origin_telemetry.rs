use crate::{
    defaults::{
        default_hls_origin_telemetry_freshness_secs, default_hls_origin_telemetry_queue_capacity,
        default_hls_origin_telemetry_window_secs, is_default_hls_origin_telemetry_freshness_secs,
        is_default_hls_origin_telemetry_queue_capacity, is_default_hls_origin_telemetry_window_secs, is_false,
    },
    error::TuliproxError,
};

/// Smallest accepted `queue_capacity`; a smaller queue would drop events under any burst.
pub const HLS_ORIGIN_TELEMETRY_MIN_QUEUE_CAPACITY: usize = 16;

/// Largest accepted `queue_capacity`: Tokio's bounded channel panics above `Semaphore::MAX_PERMITS`.
pub const HLS_ORIGIN_TELEMETRY_MAX_QUEUE_CAPACITY: usize = usize::MAX >> 3;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HlsOriginTelemetryConfigDto {
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(
        default = "default_hls_origin_telemetry_queue_capacity",
        skip_serializing_if = "is_default_hls_origin_telemetry_queue_capacity"
    )]
    pub queue_capacity: usize,
    #[serde(
        default = "default_hls_origin_telemetry_freshness_secs",
        skip_serializing_if = "is_default_hls_origin_telemetry_freshness_secs"
    )]
    pub freshness_secs: u64,
    #[serde(
        default = "default_hls_origin_telemetry_window_secs",
        skip_serializing_if = "is_default_hls_origin_telemetry_window_secs"
    )]
    pub window_secs: u64,
}

impl Default for HlsOriginTelemetryConfigDto {
    fn default() -> Self {
        Self {
            enabled: false,
            queue_capacity: default_hls_origin_telemetry_queue_capacity(),
            freshness_secs: default_hls_origin_telemetry_freshness_secs(),
            window_secs: default_hls_origin_telemetry_window_secs(),
        }
    }
}

impl HlsOriginTelemetryConfigDto {
    pub fn is_empty(&self) -> bool { *self == Self::default() }

    pub(crate) fn prepare(&self) -> Result<(), TuliproxError> {
        if !(HLS_ORIGIN_TELEMETRY_MIN_QUEUE_CAPACITY..=HLS_ORIGIN_TELEMETRY_MAX_QUEUE_CAPACITY)
            .contains(&self.queue_capacity)
        {
            return Err(TuliproxError::ConfigReverseProxy(format!(
                "`hls_origin_telemetry.queue_capacity` must be between {HLS_ORIGIN_TELEMETRY_MIN_QUEUE_CAPACITY} and {HLS_ORIGIN_TELEMETRY_MAX_QUEUE_CAPACITY}"
            )));
        }
        if self.freshness_secs == 0 || self.window_secs == 0 {
            return Err(TuliproxError::ConfigReverseProxy(
                "`hls_origin_telemetry.freshness_secs` and `window_secs` must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{HlsOriginTelemetryConfigDto, HLS_ORIGIN_TELEMETRY_MAX_QUEUE_CAPACITY};
    use crate::model::ReverseProxyConfigDto;

    #[test]
    fn absent_block_equals_default() {
        let dto: HlsOriginTelemetryConfigDto = serde_saphyr::from_str("{}").expect("empty block should parse");
        assert_eq!(dto, HlsOriginTelemetryConfigDto::default());
        assert!(dto.is_empty());
        assert!(!dto.enabled);
        assert_eq!((dto.queue_capacity, dto.freshness_secs, dto.window_secs), (1024, 60, 300));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(serde_saphyr::from_str::<HlsOriginTelemetryConfigDto>("enabled: true\nbogus: 1").is_err());
    }

    #[test]
    fn roundtrips_under_reverse_proxy() {
        let yaml = "rewrite_secret: 00112233445566778899aabbccddeeff\nhls_origin_telemetry:\n  enabled: true\n  queue_capacity: 32\n";
        let cfg: ReverseProxyConfigDto = serde_saphyr::from_str(yaml).expect("config should parse");
        let telemetry = cfg.hls_origin_telemetry.clone().expect("block should deserialize");
        assert!(telemetry.enabled);
        assert_eq!(telemetry.queue_capacity, 32);
        let serialized = serde_saphyr::to_string(&cfg).expect("serialization should succeed");
        let reparsed: ReverseProxyConfigDto = serde_saphyr::from_str(&serialized).expect("roundtrip should parse");
        assert_eq!(reparsed, cfg);
    }

    #[test]
    fn default_block_is_cleaned_away() {
        let mut cfg = ReverseProxyConfigDto {
            rewrite_secret: "00112233445566778899aabbccddeeff".to_string(),
            hls_origin_telemetry: Some(HlsOriginTelemetryConfigDto::default()),
            ..Default::default()
        };
        cfg.clean();
        assert!(cfg.hls_origin_telemetry.is_none());
    }

    #[test]
    fn prepare_validates_bounds() {
        let mut cfg = HlsOriginTelemetryConfigDto { enabled: true, queue_capacity: 15, ..Default::default() };
        assert!(cfg.prepare().is_err());
        cfg.queue_capacity = 16;
        assert!(cfg.prepare().is_ok());
        cfg.queue_capacity = HLS_ORIGIN_TELEMETRY_MAX_QUEUE_CAPACITY;
        assert!(cfg.prepare().is_ok());
        cfg.queue_capacity = HLS_ORIGIN_TELEMETRY_MAX_QUEUE_CAPACITY + 1;
        assert!(cfg.prepare().is_err());
        cfg.queue_capacity = 16;
        cfg.window_secs = 0;
        assert!(cfg.prepare().is_err());
        cfg.window_secs = 300;
        cfg.freshness_secs = 0;
        assert!(cfg.prepare().is_err());
    }
}
