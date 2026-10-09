use super::{ActiveProviderManager, ConnectionKind, PlaybackLeaseRef};

mod admission;
mod behavior;
mod lifecycle;
mod retry;
mod support;
mod terminal;

use self::support::{
    acquire_live_hls_from_lineup, alias_pool, build_test_app_config, confirmed_alias_request,
    create_test_app_config_single_provider_pool, create_test_app_config_single_unlimited_provider_pool,
    create_test_app_config_with_dual_provider_pool, create_test_app_config_with_pool,
    live_hls_playback_on_alias_with_lapsed_lease,
};
