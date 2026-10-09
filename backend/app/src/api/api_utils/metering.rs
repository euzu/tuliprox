use crate::api::model::AppState;
use shared::model::PlaylistItemType;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct StreamMeteringConfig {
    pub(super) meter_uid: u32,
    pub(super) meter_stream: bool,
    pub(super) pending_shared_registration: Option<tuliprox_session::PendingSharedMeterRegistration>,
}

impl StreamMeteringConfig {
    pub(super) fn commit_shared_registration(&mut self) {
        if let Some(registration) = self.pending_shared_registration.take() {
            registration.commit();
        }
    }
}

pub(super) fn is_throttled_stream(item_type: PlaylistItemType, throttle_kbps: usize) -> bool {
    throttle_kbps > 0
        && matches!(
            item_type,
            PlaylistItemType::Video
                | PlaylistItemType::Series
                | PlaylistItemType::SeriesInfo
                | PlaylistItemType::Catchup
                | PlaylistItemType::LocalVideo
                | PlaylistItemType::LocalSeries
                | PlaylistItemType::LocalSeriesInfo
        )
}

pub(super) fn get_stream_throttle(app_state: &Arc<AppState>) -> u64 {
    app_state
        .app_config
        .config
        .load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map(|stream| stream.throttle_kbps)
        .unwrap_or_default()
}

pub(super) fn is_stream_metrics_enabled(app_state: &Arc<AppState>) -> bool {
    app_state
        .app_config
        .config
        .load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .is_some_and(|stream| stream.metrics_enabled)
}

pub(super) fn prepare_stream_metering(
    app_state: &Arc<AppState>,
    stream_url: &str,
    share_stream: bool,
    has_stream: bool,
    has_deferred_provider_open: bool,
) -> StreamMeteringConfig {
    if !is_stream_metrics_enabled(app_state) {
        return StreamMeteringConfig::default();
    }

    if share_stream {
        let (meter_uid, pending_registration) = app_state
            .shared_stream_manager
            .reserve_meter_uid(stream_url, || app_state.connection_manager.next_stream_uid());
        return StreamMeteringConfig {
            meter_uid,
            meter_stream: has_stream || has_deferred_provider_open,
            pending_shared_registration: pending_registration,
        };
    } else if has_stream || has_deferred_provider_open {
        let meter_uid = app_state.connection_manager.next_stream_uid();
        return StreamMeteringConfig { meter_uid, meter_stream: true, pending_shared_registration: None };
    }

    StreamMeteringConfig::default()
}

pub(super) fn resolve_stream_config_u64(
    stream_config: Option<&crate::model::StreamConfig>,
    selector: impl FnOnce(&crate::model::StreamConfig) -> u64,
    default_value: u64,
) -> u64 {
    stream_config.map_or(default_value, selector)
}

pub(super) fn get_stream_config_u64(
    app_state: &Arc<AppState>,
    selector: impl FnOnce(&crate::model::StreamConfig) -> u64,
    default_value: u64,
) -> u64 {
    let config = app_state.app_config.config.load();
    let stream_config = config.reverse_proxy.as_ref().and_then(|reverse_proxy| reverse_proxy.stream.as_ref());
    resolve_stream_config_u64(stream_config, selector, default_value)
}
