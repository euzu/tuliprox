use crate::api::model::ProviderIdType;
use shared::model::{LiveStreamProperties, SeriesStreamProperties, VideoStreamProperties};
use std::mem;

const BATCH_THRESHOLD: usize = 200;

#[derive(Debug, Default)]
pub struct BatchResultCollector {
    pub(crate) vod: Vec<(ProviderIdType, VideoStreamProperties)>,
    pub(crate) series: Vec<(ProviderIdType, SeriesStreamProperties)>,
    pub(crate) live: Vec<(ProviderIdType, LiveStreamProperties)>,
}

impl BatchResultCollector {
    pub fn new() -> Self {
        Self {
            vod: Vec::with_capacity(BATCH_THRESHOLD),
            series: Vec::with_capacity(BATCH_THRESHOLD),
            live: Vec::with_capacity(BATCH_THRESHOLD),
        }
    }

    pub fn add_vod(&mut self, id: ProviderIdType, props: VideoStreamProperties) { self.vod.push((id, props)); }

    pub fn add_series(&mut self, id: ProviderIdType, props: SeriesStreamProperties) { self.series.push((id, props)); }

    pub fn add_live(&mut self, id: ProviderIdType, props: LiveStreamProperties) { self.live.push((id, props)); }

    pub fn should_flush(&self) -> bool {
        self.vod.len() >= BATCH_THRESHOLD || self.series.len() >= BATCH_THRESHOLD || self.live.len() >= BATCH_THRESHOLD
    }

    pub fn take_vod_updates(&mut self) -> Vec<(ProviderIdType, VideoStreamProperties)> {
        if self.vod.is_empty() {
            Vec::new()
        } else {
            mem::take(&mut self.vod)
        }
    }

    pub fn take_series_updates(&mut self) -> Vec<(ProviderIdType, SeriesStreamProperties)> {
        if self.series.is_empty() {
            Vec::new()
        } else {
            mem::take(&mut self.series)
        }
    }

    pub fn take_live_updates(&mut self) -> Vec<(ProviderIdType, LiveStreamProperties)> {
        if self.live.is_empty() {
            Vec::new()
        } else {
            mem::take(&mut self.live)
        }
    }

    pub fn is_empty(&self) -> bool { self.vod.is_empty() && self.series.is_empty() && self.live.is_empty() }
}
