use super::resolve_provider_url_for_request;

mod admission;
mod behavior;
mod configuration;
mod lifecycle;
mod persistence;
mod protocol;
mod support;

use self::support::{
    epg_dt, ics_source_dto, playlist_update_target, test_app_config, test_app_state,
    test_app_state_with_manual_update_sender, xmltv_source_dto,
};

mod behavior_epg_preview;
mod behavior_manual_update;
mod behavior_provider_resolution;
mod behavior_recording_routes;
