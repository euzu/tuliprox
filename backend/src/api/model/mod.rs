mod active_provider_manager;
mod active_user_manager;
mod admission_strategy;
mod app_state;
mod byte_range;
mod connection_manager;
mod download;
mod event_manager;
mod hls_cache;
mod hls_provisioning;
mod metadata_update_manager;
mod model_utils;
mod playlist_mem_cache;
mod provider_config;
mod provider_dns_manager;
mod provider_lineup_manager;
mod proxy;
mod qos_aggregation_manager;
mod recording_worker;
mod request;
mod stream;
mod stream_error;
mod streams;
mod update_guard;
mod xtream;

#[cfg(test)]
pub(in crate::api) use self::hls_provisioning::{
    build_hls_custom_video_manifest_body, hls_panel_provisioning_manifest_path,
};
pub use self::{
    active_provider_manager::*, app_state::*, connection_manager::*, event_manager::*, hls_cache::*,
    hls_provisioning::HlsProvisioningState, metadata_update_manager::*, playlist_mem_cache::*, provider_dns_manager::*,
    provider_lineup_manager::*, proxy::*, stream::*, update_guard::*,
};
pub(in crate::api) use self::{
    active_user_manager::*,
    admission_strategy::{evaluate_strategy, AdmissionDecision, EvictionCandidate, GraceMode, StrategyContext},
    byte_range::{resolve_single_byte_range, SingleByteRange},
    download::*,
    hls_provisioning::{
        hls_custom_video_manifest_response_for_access_lease,
        hls_custom_video_manifest_response_with_virtual_id,
        hls_provisioning_discontinuity_sequence, hls_virtual_entry_redirect_response,
        parse_hls_panel_provisioning_segment_route_name, start_hls_panel_provisioning_once,
        try_hls_panel_provisioning_manifest_response, HlsPanelProvisioningRedirectPaths, HlsProvisioningStatus,
    },
    model_utils::*,
    provider_config::*,
    qos_aggregation_manager::*,
    recording_worker::*,
    request::*,
    stream_error::*,
    xtream::*,
};
pub(crate) use self::{
    hls_cache::{
        log_hls_origin_content_coding, HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource,
        HlsPostRefreshRuntime,
    },
    streams::*,
};
mod batch_result_collector;
pub use self::batch_result_collector::*;
