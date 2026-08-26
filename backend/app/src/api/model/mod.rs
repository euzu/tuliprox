mod app_state;
mod app_state_view;
mod hls_provisioning;
mod proxy;
mod streams;

#[cfg(test)]
pub(in crate::api) use self::hls_provisioning::{
    build_hls_custom_video_manifest_body, hls_panel_provisioning_manifest_path,
};
pub use self::{
    app_state::*, app_state_view::*, hls_provisioning::HlsProvisioningState,
    proxy::*,
};
// In-memory playlist storage moved to `repository`; re-exported so `api` call
// sites keep their names.
pub use crate::repository::playlist_mem_cache::*;
// Update semaphores moved to `model`; re-exported so `api` keeps its names.
pub use crate::model::update_guard::*;
pub use crate::model::update_task::*;
// Provider value types moved to `model`; re-exported so `api` keeps its names.
pub use crate::model::provider::*;
// HTTP range parsing moved to `tuliprox_core::utils`; re-exported so `api` call
// sites keep their names.
pub use tuliprox_core::utils::byte_range::{resolve_single_byte_range, SingleByteRange};
// Dependency-free model types moved to `tuliprox-core`, and the provider
// response-header helpers to `tuliprox-session` beside the header types they
// operate on. Re-exported so `api` call sites keep their names.
pub use tuliprox_core::model::{
    batch_result_collector::*, user_api_request::*, xtream_response::*,
};
pub use tuliprox_session::response_headers::*;
// Background workers moved to the layers they read: provider DNS and QoS
// aggregation to `tuliprox-session`, the playlist cache loader to
// `tuliprox-repository`. Re-exported so `api` call sites keep their names.
pub use tuliprox_repository::playlist_cache_loader::*;
pub use tuliprox_session::{provider_dns_manager::*, qos_aggregation_manager::*};
// Background metadata resolution moved to `tuliprox-metadata`; re-exported so
// `api` call sites keep their names.
pub use tuliprox_metadata::{ctx::MetadataUpdateCtx, manager::*};
pub use crate::model::stream_error::*;

// The recording queue and the DVR moved to `tuliprox-dvr`; re-exported so `api`
// call sites keep their names, module paths included.
pub use tuliprox_dvr::{download, recording};
pub use tuliprox_dvr::{download::*, recording::*};

// Provider allocation and the streaming-session runtime moved to
// `tuliprox-session`; re-exported so `api` call sites keep their names, module
// paths included.
pub use tuliprox_session::{
    active_provider_manager, active_user_manager, admission_strategy, connection_manager,
    event_manager, meter, provider_lineup_manager, stream,
};
pub use tuliprox_session::{
    active_provider_manager::*, active_user_manager::*, admission_strategy::*,
    connection_manager::*, event_manager::*, meter::*, provider_lineup_manager::*, stream::*,
    streams::*,
};
pub(in crate::api) use self::{
    hls_provisioning::{
        hls_custom_video_manifest_response_for_access_lease,
        hls_custom_video_manifest_response_with_virtual_id,
        hls_provisioning_discontinuity_sequence, hls_virtual_entry_redirect_response,
        parse_hls_panel_provisioning_segment_route_name, start_hls_panel_provisioning_once,
        try_hls_panel_provisioning_manifest_response, HlsPanelProvisioningRedirectPaths, HlsProvisioningStatus,
    },
};
pub(crate) use self::streams::*;
// The HLS proxy moved to `tuliprox-hls`; re-exported so `api` call sites keep
// their names, module path included.
pub use tuliprox_hls as hls_cache;
pub use tuliprox_hls::*;
