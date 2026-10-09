use crate::{
    error::TuliproxError,
    model::{ByteSize, Secs},
};

pub const DEFAULT_HLS_PROGRESSIVE_BYTES_PER_SEGMENT: &str = "32MB";
pub const DEFAULT_HLS_PROGRESSIVE_BYTES_TOTAL: &str = "128MB";

#[derive(Debug, Clone, Copy, Hash, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HlsStartupMode {
    #[default]
    Conservative,
    FirstReady,
    Progressive,
}

crate::impl_str_enum!(HlsStartupMode, "HLS startup mode",
    Conservative => "conservative",
    FirstReady => "first_ready",
    Progressive => "progressive",
);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct HlsStartupConfigDto {
    pub mode: HlsStartupMode,
    pub max_progressive_segments: usize,
    pub max_progressive_bytes_per_segment: ByteSize,
    pub max_progressive_bytes_total: ByteSize,
    pub max_progressive_reader_lifetime_secs: Secs,
    pub max_deferred_repairs: usize,
}

impl Default for HlsStartupConfigDto {
    fn default() -> Self {
        Self {
            mode: HlsStartupMode::Conservative,
            max_progressive_segments: 16,
            max_progressive_bytes_per_segment: ByteSize::new(DEFAULT_HLS_PROGRESSIVE_BYTES_PER_SEGMENT),
            max_progressive_bytes_total: ByteSize::new(DEFAULT_HLS_PROGRESSIVE_BYTES_TOTAL),
            max_progressive_reader_lifetime_secs: Secs::new(90),
            max_deferred_repairs: 32,
        }
    }
}

impl HlsStartupConfigDto {
    pub fn is_empty(&self) -> bool { self == &Self::default() }

    pub fn prepare(&self) -> Result<(), TuliproxError> {
        let segment =
            self.max_progressive_bytes_per_segment.parse_bytes().map_err(TuliproxError::ConfigReverseProxy)?.get();
        let total = self.max_progressive_bytes_total.parse_bytes().map_err(TuliproxError::ConfigReverseProxy)?.get();
        if self.max_progressive_segments == 0
            || self.max_deferred_repairs == 0
            || self.max_progressive_reader_lifetime_secs.get() == 0
            || segment == 0
            || total < segment
        {
            return Err(TuliproxError::ConfigReverseProxy(
                "hls_cache.startup requires positive limits and a total byte budget at least as large as the segment budget".to_string()
            ));
        }
        Ok(())
    }
}
