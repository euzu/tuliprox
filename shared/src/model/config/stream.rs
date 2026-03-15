use crate::{
    error::{info_err_res, TuliproxError, TuliproxErrorKind},
    utils::{
        default_as_true, default_catchup_session_ttl_secs, default_grace_period_millis,
        default_grace_period_timeout_secs, default_hls_session_ttl_secs, default_shared_burst_buffer_mb,
        is_blank_optional_string, is_default_catchup_session_ttl_secs, is_default_grace_period_millis,
        is_default_grace_period_timeout_secs, is_default_hls_session_ttl_secs, is_default_shared_burst_buffer_mb,
        is_true, parse_to_kbps,
    },
};

const STREAM_QUEUE_SIZE: usize = 1024; // mpsc channel holding messages. with 8192byte chunks and 2Mbit/s approx 8MB
const MIN_SHARED_BURST_BUFFER_MB: u64 = 1;

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
    /// If true (default), wait for grace period check before streaming.
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
}

impl Default for StreamConfigDto {
    fn default() -> Self {
        StreamConfigDto {
            retry: true,
            buffer: None,
            throttle: None,
            grace_period_millis: default_grace_period_millis(),
            grace_period_timeout_secs: default_grace_period_timeout_secs(),
            throttle_kbps: 0,
            shared_burst_buffer_mb: default_shared_burst_buffer_mb(),
            grace_period_hold_stream: true,
            hls_session_ttl_secs: default_hls_session_ttl_secs(),
            catchup_session_ttl_secs: default_catchup_session_ttl_secs(),
        }
    }
}

impl StreamConfigDto {
    pub fn is_empty(&self) -> bool {
        self.retry
            && (self.buffer.is_none() || self.buffer.as_ref().is_some_and(|b| b.is_empty()))
            && (self.throttle.is_none() || self.throttle.as_ref().is_some_and(|t| t.is_empty()))
            && self.grace_period_millis == default_grace_period_millis()
            && self.grace_period_timeout_secs == default_grace_period_timeout_secs()
            && self.throttle_kbps == 0
            && self.shared_burst_buffer_mb == default_shared_burst_buffer_mb()
            && self.grace_period_hold_stream
            && self.hls_session_ttl_secs == default_hls_session_ttl_secs()
            && self.catchup_session_ttl_secs == default_catchup_session_ttl_secs()
    }

    pub(crate) fn prepare(&mut self) -> Result<(), TuliproxError> {
        if let Some(buffer) = self.buffer.as_mut() {
            buffer.prepare();
        }
        if let Some(throttle) = &self.throttle {
            parse_to_kbps(throttle).map_err(|err| TuliproxError::new(TuliproxErrorKind::Info, err))?;
        } else {
            self.throttle_kbps = 0;
        }

        if self.grace_period_millis > 0 {
            if self.grace_period_timeout_secs == 0 {
                let triple_ms = self.grace_period_millis.saturating_mul(3);
                self.grace_period_timeout_secs = std::cmp::max(1, triple_ms.div_ceil(1000));
            } else if self.grace_period_millis / 1000 > self.grace_period_timeout_secs {
                return info_err_res!(
                    "Grace time period timeout {} sec should be more than grace time period {} ms",
                    self.grace_period_timeout_secs,
                    self.grace_period_millis
                );
            }
        }

        if self.shared_burst_buffer_mb < MIN_SHARED_BURST_BUFFER_MB {
            return info_err_res!("`shared_burst_buffer_mb` must be at least {MIN_SHARED_BURST_BUFFER_MB} MB");
        }

        Ok(())
    }
}
