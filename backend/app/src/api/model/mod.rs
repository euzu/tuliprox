mod app_state;
mod hls_provisioning;
mod metadata_update_manager;
mod model_utils;
mod provider_dns_manager;
mod proxy;
mod qos_aggregation_manager;
mod request;
mod streams;
mod xtream;

#[cfg(test)]
pub(in crate::api) use self::hls_provisioning::{
    build_hls_custom_video_manifest_body, hls_panel_provisioning_manifest_path,
};
pub use self::{
    app_state::*, hls_provisioning::HlsProvisioningState, metadata_update_manager::*,
    provider_dns_manager::*, proxy::*,
};
mod playlist_cache_loader;
pub use self::playlist_cache_loader::*;
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
    model_utils::*,
    qos_aggregation_manager::*,
    request::*,
    xtream::*,
};
pub(crate) use self::streams::*;
// The HLS proxy moved to `tuliprox-hls`; re-exported so `api` call sites keep
// their names, module path included.
pub use tuliprox_hls as hls_cache;
pub use tuliprox_hls::*;

mod batch_result_collector;
pub use self::batch_result_collector::*;
